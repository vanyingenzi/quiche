// Copyright (C) 2020, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! This module implements utulities in order to support multicore on top of QUICHE
//! Author: Vany Ingenzi

use crate::args::*;
use crate::client::*;
use crate::common::*;
extern crate core_affinity;

use shared_arena::ArenaBox;
use static_assertions;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::ToSocketAddrs;
use std::net::{IpAddr, SocketAddr};

use std::io::prelude::*;

use std::rc::Rc;

use std::cell::RefCell;
use std::str::FromStr;
use std::time::Instant;
use lockfree::channel::{mpsc, spsc};
use std::thread;
use std::time::Duration;

use mio::net::UdpSocket;
use quiche::Connection;
use ring::rand::*;

// const MAX_BUF_SIZE: usize = 65507;
const MAX_DATAGRAM_SIZE: usize = 1350;

use shared_arena::SharedArena;
use std::sync::Arc;

type Packet = [u8; MAX_DATAGRAM_SIZE];

pub struct MRcvPacketInfo {
    pub local_addr: SocketAddr,
    pub len: usize,
    pub rcv_info: quiche::RecvInfo, 
    pub pckt: ArenaBox<Packet>,
}

pub struct MSndPacketInfo {
    pub peer_addr: SocketAddr,
    pub len: usize,
    pub pkt: ArenaBox<Packet>
}

fn multicore_create_socket(
    addrs: (&SocketAddr, &SocketAddr), poll: &mut mio::Poll,
) -> UdpSocket {
    let (local_addr, _) = addrs;
    let mut socket = mio::net::UdpSocket::bind(*local_addr).unwrap();
    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();
    socket
}

fn client_thread(
    conn_channel: mpsc::Sender<MRcvPacketInfo>,
    mut path_channel: spsc::Receiver<MSndPacketInfo>,
    addrs: (SocketAddr, SocketAddr),
    arena: Arc<SharedArena<Packet>>
) -> Result<(), ClientError> {
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);
    let (local_addr, peer_addr) = addrs;
    let socket = multicore_create_socket((&local_addr, &peer_addr), &mut poll);

    info!(
        "[Path Thread]: Thread {:?}, started with path {} <-> {}",
        thread::current().id(),
        local_addr,
        peer_addr
    );

    loop {
        poll.poll(&mut events, Some(Duration::ZERO)).unwrap();
        for _event in &events {
            'read: loop {
                let mut packet: Packet = [0u8; MAX_DATAGRAM_SIZE];
                let (len, from) = match socket.recv_from(&mut packet) {
                    Ok(v) => v,

                    Err(e) => {
                        // There are no more UDP packets to read on this socket.
                        // Process subsequent events.
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            trace!("{}: recv() would block", local_addr);
                            break 'read;
                        }
                        return Err(ClientError::Other(format!(
                            "{local_addr}: recv() failed: {e:?}"
                        )));
                    },
                };

                let packet_box = arena.alloc(packet);

                let rcv_info = quiche::RecvInfo {
                    to: local_addr, 
                    from
                };
                conn_channel.send(MRcvPacketInfo { local_addr, len, rcv_info, pckt: packet_box }).ok();
            }
        }
        
        match path_channel.recv(){
            Ok(mut pkt) => {
                if let Err(e) = socket.send_to(&mut (pkt.pkt[..pkt.len]), pkt.peer_addr) {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!(
                            "{} -> {}: send() would block",
                            local_addr,
                            peer_addr
                        );
                    }
                    return Err(ClientError::Other(format!(
                        "{} -> {}: send() failed: {:?}",
                        local_addr, peer_addr, e
                    )));
                }

                trace!("{} -> {}: written {}", local_addr, peer_addr, pkt.pkt.len());
            }, 
            Err(_) => {}
        }
    }
}

pub fn multicore_connect(
    args: ClientArgs, conn_args: CommonArgs,
    output_sink: impl FnMut(String) + 'static,
) -> Result<(), ClientError> {
    static_assertions::assert_impl_all!(Connection: Send);

    // ** [Connection thread]

    let mut buf = [0; 65535];

    let output_sink =
        Rc::new(RefCell::new(output_sink)) as Rc<RefCell<dyn FnMut(_)>>;

    // We'll only connect to the first server provided in URL list.
    let connect_url = &args.urls[0];

    // Resolve server address.
    let peer_addr = if let Some(addr) = &args.connect_to {
        addr.parse().expect("--connect-to is expected to be a string containing an IPv4 or IPv6 address with a port. E.g. 192.0.2.0:443")
    } else {
        connect_url.to_socket_addrs().unwrap().next().unwrap()
    };

    let (sockets_addrs, local_addr) = multicore_prepare_addresses(&peer_addr, &args);

    let mut addrs = Vec::with_capacity(sockets_addrs.len());
    addrs.push(local_addr);
    for (src, _) in sockets_addrs.iter() {
        if *src != local_addr {
            addrs.push(*src);
        }
    }
    
    // Warn the user if there are more usable addresses than the advertised
    // `active_connection_id_limit`.
    if addrs.len() as u64 > conn_args.max_active_cids {
        warn!(
            "{} addresses provided, but configuration restricts to at most {} \
               active CIDs; increase the --max-active-cids parameter to use all \
               the provided addresses",
            addrs.len(),
            conn_args.max_active_cids
        );
    }

    let mut rm_addrs = args.rm_addrs.clone();
    let mut status = args.status.clone();

    // Create the configuration for the QUIC connection.
    let mut config = quiche::Config::new(args.version).unwrap();

    if let Some(ref trust_origin_ca_pem) = args.trust_origin_ca_pem {
        config
            .load_verify_locations_from_file(trust_origin_ca_pem)
            .map_err(|e| {
                ClientError::Other(format!(
                    "error loading origin CA file : {}",
                    e
                ))
            })?;
    } else {
        config.verify_peer(!args.no_verify);
    }

    config.set_application_protos(&conn_args.alpns).unwrap();

    config.set_max_idle_timeout(conn_args.idle_timeout);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(conn_args.max_data);
    config.set_initial_max_stream_data_bidi_local(conn_args.max_stream_data);
    config.set_initial_max_stream_data_bidi_remote(conn_args.max_stream_data);
    config.set_initial_max_stream_data_uni(conn_args.max_stream_data);
    config.set_initial_max_streams_bidi(conn_args.max_streams_bidi);
    config.set_initial_max_streams_uni(conn_args.max_streams_uni);
    config.set_disable_active_migration(!conn_args.enable_active_migration);
    config.set_active_connection_id_limit(conn_args.max_active_cids);
    config.set_multipath(conn_args.multipath);

    config.set_max_connection_window(conn_args.max_window);
    config.set_max_stream_window(conn_args.max_stream_window);

    let mut keylog = None;

    if let Some(keylog_path) = std::env::var_os("SSLKEYLOGFILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(keylog_path)
            .unwrap();

        keylog = Some(file);

        config.log_keys();
    }

    if conn_args.no_grease {
        config.grease(false);
    }

    if conn_args.early_data {
        config.enable_early_data();
    }

    config
        .set_cc_algorithm_name(&conn_args.cc_algorithm)
        .unwrap();

    if conn_args.disable_hystart {
        config.enable_hystart(false);
    }

    if conn_args.dgrams_enabled {
        config.enable_dgram(true, 1000, 1000);
    }

    let mut http_conn: Option<Box<dyn HttpConn>> = None;

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);
    // Create a QUIC connection and initiate handshake.
    let (mut conn, mut conn_paths) = quiche::connect(
        connect_url.domain(),
        &scid,
        local_addr,
        peer_addr,
        &mut config,
    ).unwrap();

    
    if let Some(keylog) = &mut keylog {
        if let Ok(keylog) = keylog.try_clone() {
            conn.set_keylog(Box::new(keylog));
        }
    }

    // Only bother with qlog if the user specified it.
    #[cfg(feature = "qlog")]
    {
        if let Some(dir) = std::env::var_os("QLOGDIR") {
            let id = format!("{scid:?}");
            let writer = make_qlog_writer(&dir, "client", &id);
            conn.set_qlog(
                std::boxed::Box::new(writer),
                "quiche-client qlog".to_string(),
                format!("{} id={}", "quiche-client qlog", id),
            );
        }
    }

    if let Some(session_file) = &args.session_file {
        if let Ok(session) = std::fs::read(session_file) {
            conn.set_session(&mut conn_paths.get_active_mut().unwrap(), &session).ok();
        }
    }

    let core_ids = core_affinity::get_core_ids().unwrap();
    let mut current_path_thread_idx = 0;
    let arena = Arc::new(SharedArena::new());

    let (conn_tx, mut conn_rx) = mpsc::create();
    let mut path_to_channel = BTreeMap::new();

    for (local, peer) in sockets_addrs.clone() {
        let (path_tx, path_rx) = spsc::create();
        path_to_channel.insert((local, peer), path_tx);
        let clone_tx = conn_tx.clone();
        // HTTPConn doesn't implement Send therefore can't be used in a Mutex
        let core_id = core_ids[current_path_thread_idx];
        let arena_clone = arena.clone();
        thread::spawn(move || {
            if core_affinity::set_for_current(core_id) {
                info!("Set core affinity for {:?}", thread::current().id());
            }
            trace!(
                "[Thread {:?}] spawned with path {:?} <-> {:?}",
                thread::current().id(),
                local,
                peer
            );
            client_thread(
                clone_tx,
                path_rx,
                (local, peer),
                arena_clone
            )
            .unwrap();
        });
        current_path_thread_idx += 1;
    }

    info!(
        "connecting to {:} from {:} with scid {:?}",
        peer_addr, local_addr, scid,
    );

    let mut pkt: Packet = [0u8; MAX_DATAGRAM_SIZE];
    let (len, send_info) = conn.send(&mut conn_paths, &mut pkt).expect("initial send failed");
    let channel = path_to_channel.get_mut(&(send_info.from, send_info.to)).unwrap();
    channel.send(MSndPacketInfo { peer_addr: send_info.to, len, pkt: arena.alloc(pkt) }).ok();

    trace!("written {}", len);

    let mut new_path_probed = false;
    let mut probed_paths = 0;
    let app_data_start = std::time::Instant::now();
    let mut scid_sent = false;
    let mut migrated = false;
    let mut app_proto_selected: bool = false;
    let mut pkt_count = 0;

    let mut optional_from_queue = None;
    
    loop {

        if !conn.is_in_early_data() || app_proto_selected {
            // poll.poll(&mut events, conn.timeout(&mut conn_paths)).unwrap();
            match conn_rx.recv() {
                Ok(v) => optional_from_queue = Some(v), 
                Err(_) => {
                    optional_from_queue = None;
                }
            }
        }

        match optional_from_queue {
            Some(ref mut rcv_pkt) => {
                // Read incoming UDP packets from the socket and feed them to quiche,
                // until there are no more packets to read.
                // let rcv_instant = std::time::Instant::now();
                let len = rcv_pkt.pckt.len();
                if let Some(target_path) = conn_args.dump_packet_path.as_ref() {
                    let path = format!("{target_path}/{pkt_count}.pkt");
                    
                    if let Ok(f) = std::fs::File::create(path) {
                        let mut f = std::io::BufWriter::new(f);
                        f.write_all(&rcv_pkt.pckt[..len]).ok();
                    }
                }
        
                pkt_count += 1;
                
                // Process potentially coalesced packets.
                let read = match conn.recv(&mut conn_paths, &mut rcv_pkt.pckt[..rcv_pkt.len], rcv_pkt.rcv_info) {
                    Ok(v) => v,
        
                    Err(e) => {
                        error!("{}: recv failed: {:?}", local_addr, e);
                        return Err(ClientError::Other(format!("{:?}", e)));
                    },
                };

                trace!("{}: processed {} bytes", local_addr, read);
            }, 
            None => {
                // If the event loop reported no events, it means that the timeout
                // has expired, so handle it without attempting to read packets. We
                // will then proceed with the send loop.
                // trace!("timed out");
                conn.on_timeout(&mut conn_paths);
            }
        };


        if conn.is_closed() {
            info!(
                "connection closed, {:?} {:?}",
                conn.stats(&mut conn_paths),
                conn_paths.iter().map(|(_, p)| p.stats()).collect::<Vec<quiche::PathStats>>()
            );

            if !conn.is_established() {
                error!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            if let Some(session_file) = &args.session_file {
                if let Some(session) = conn.session() {
                    std::fs::write(session_file, session).ok();
                }
            }

            if let Some(h_conn) = http_conn {
                if h_conn.report_incomplete(&app_data_start) {
                    return Err(ClientError::HttpFail);
                }
            }

            break;
        }

        // Create a new application protocol session once the QUIC connection is
        // established.
        if (conn.is_established() || conn.is_in_early_data()) &&
            (!args.perform_migration || migrated) &&
            !app_proto_selected
        {
            // At this stage the ALPN negotiation succeeded and selected a
            // single application protocol name. We'll use this to construct
            // the correct type of HttpConn but `application_proto()`
            // returns a slice, so we have to convert it to a str in order
            // to compare to our lists of protocols. We `unwrap()` because
            // we need the value and if something fails at this stage, there
            // is not much anyone can do to recover.

            let app_proto = conn.application_proto();

            if alpns::HTTP_09.contains(&app_proto) {
                http_conn = Some(Http09Conn::with_urls(
                    &args.urls,
                    args.reqs_cardinal,
                    Rc::clone(&output_sink),
                ));

                app_proto_selected = true;
            } else if alpns::HTTP_3.contains(&app_proto) {
                let dgram_sender = if conn_args.dgrams_enabled {
                    Some(Http3DgramSender::new(
                        conn_args.dgram_count,
                        conn_args.dgram_data.clone(),
                        0,
                    ))
                } else {
                    None
                };

                http_conn = Some(Http3Conn::with_urls(
                    &mut conn,
                    &args.urls,
                    args.reqs_cardinal,
                    &args.req_headers,
                    &args.body,
                    &args.method,
                    args.send_priority_update,
                    conn_args.max_field_section_size,
                    conn_args.qpack_max_table_capacity,
                    conn_args.qpack_blocked_streams,
                    args.dump_json,
                    dgram_sender,
                    Rc::clone(&output_sink),
                ));

                app_proto_selected = true;
            }
        }

        // If we have an HTTP connection, first issue the requests then
        // process received data.
        if let Some(h_conn) = http_conn.as_mut() {
            h_conn.send_requests(&mut conn, &mut conn_paths, &args.dump_response_path);
            h_conn.handle_responses(&mut conn, &mut conn_paths, &mut buf, &app_data_start);
        }

        for (_, path) in conn_paths.iter_mut(){
            conn.get_pending_path_events(path);
        }

        // Handle path events.
        // Handle path events.
        let paths_sockets_addrs: Vec<(std::net::SocketAddr, std::net::SocketAddr)> = conn_paths.iter().map(|(_, p)| (p.local_addr(), p.peer_addr())).collect();
        for (local_addr, peer_addr) in paths_sockets_addrs{
            loop {
                match conn_paths.pop_event(local_addr, peer_addr) {
                    Some(qe) => {
                        match qe {
                            quiche::PathEvent::New(..) => unreachable!(),
            
                            quiche::PathEvent::Validated(local_addr, peer_addr) => {
                                debug!(
                                    "Path ({}, {}) is now validated",
                                    local_addr, peer_addr
                                );
                                if conn_paths.multipath() {
                                    conn_paths.set_active(local_addr, peer_addr, true).ok();
                                } else if args.perform_migration {
                                    conn.migrate(&mut conn_paths, local_addr, peer_addr).unwrap();
                                    migrated = true;
                                }
                            },
            
                            quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                                debug!(
                                    "Path ({}, {}) failed validation",
                                    local_addr, peer_addr
                                );
                            },
            
                            quiche::PathEvent::Closed(local_addr, peer_addr, e, reason) => {
                                debug!(
                                    "Path ({}, {}) is now closed and unusable; err = {}, reason = {:?}",
                                    local_addr, peer_addr, e, reason
                                );
                            },
            
                            quiche::PathEvent::ReusedSourceConnectionId(
                                cid_seq,
                                old,
                                new,
                            ) => {
                                debug!(
                                    "Peer reused cid seq {} (initially {:?}) on {:?}",
                                    cid_seq, old, new
                                );
                            },
            
                            quiche::PathEvent::PeerMigrated(..) => unreachable!(),
            
                            quiche::PathEvent::PeerPathStatus(..) => {},
                            
                            quiche::PathEvent::PacketNumSpaceDiscarded((local, peer), epoch, handshake_status, now) => {
                                let p = conn_paths.get_mut_from_addr(local, peer).unwrap();
                                p.recovery.on_pkt_num_space_discarded(epoch, handshake_status, now);
                            },
                        }
                    }, 
                    None => break
                }
            }
        }

        // See whether source Connection IDs have been retired.
        while let Some(retired_scid) = conn.retired_scid_next() {
            info!("Retiring source CID {:?}", retired_scid);
        }

        // Provides as many CIDs as possible.
        while conn.source_cids_left() > 0 {
            let (scid, reset_token) = generate_cid_and_reset_token(&rng);

            if conn.new_source_cid(&scid, reset_token, false).is_err() {
                break;
            }

            scid_sent = true;
        }

        // Handle Multipath Issue

        if conn_args.multipath
            && probed_paths < addrs.len()
            && conn.available_dcids() > 0
            && conn.probe_path(&mut conn_paths, addrs[probed_paths], peer_addr).is_ok()
        {
            probed_paths += 1;
        }

        if !conn_args.multipath
            && args.perform_migration
            && !new_path_probed
            && scid_sent
            && conn.available_dcids() > 0
        {
            let additional_local_addr = sockets_addrs[1].0;
            conn.probe_path(&mut conn_paths,additional_local_addr, peer_addr).unwrap();
            new_path_probed = true;
        }

        if conn_paths.multipath() {
            rm_addrs.retain(|(d, addr)| {
                if app_data_start.elapsed() >= *d {
                    debug!("Abandoning path {:?}", addr);
                    conn_paths.abandon_path(
                        *addr,
                        peer_addr,
                        0,
                        "do not use me anymore".to_string().into_bytes(),
                    )
                    .is_err()
                } else {
                    true
                }
            });

            // TODO propagate these changes to path threads
            status.retain(|(d, addr, available)| {
                if app_data_start.elapsed() >= *d {
                    let status = (*available).into();
                    debug!("Advertising path status {status:?} to {addr:?}");
                    conn_paths.set_path_status( *addr, peer_addr, status, true)
                        .is_err()
                } else {
                    true
                }
            });
        }

        // Determine in which order we are going to iterate over paths.
        let scheduled_tuples = lowest_latency_scheduler(&conn_paths);

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        for (local_addr, peer_addr) in scheduled_tuples {
            let channel = path_to_channel.get_mut(&(local_addr, peer_addr)).unwrap();
            loop {
                let mut packet: Packet = [0u8; MAX_DATAGRAM_SIZE];
                let (len, send_info) = match conn.send_on_path(
                    &mut conn_paths,
                    &mut packet,
                    Some(local_addr),
                    Some(peer_addr),
                ) {
                    Ok(v) => v,

                    Err(quiche::Error::Done) => {
                        // trace!("{} -> {}: done writing", local_addr, peer_addr);
                        break;
                    },

                    Err(e) => {
                        error!(
                            "{} -> {}: send failed: {:?}",
                            local_addr, peer_addr, e
                        );

                        conn.close(false, 0x1, b"fail").ok();
                        break;
                    },
                };

                channel.send(MSndPacketInfo { peer_addr: peer_addr, pkt: arena.alloc(packet), len}).ok();
                trace!("{} -> {}: written {}", local_addr, send_info.to, len);
            }
        }


        if conn.is_closed() {
            info!(
                "connection closed, {:?} {:?}",
                conn.stats(&mut conn_paths),
                conn_paths.iter().map(|(_, p)| p.stats()).collect::<Vec<quiche::PathStats>>()
            );

            if !conn.is_established() {
                error!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            if let Some(session_file) = &args.session_file {
                if let Some(session) = conn.session() {
                    std::fs::write(session_file, session).ok();
                }
            }

            if let Some(h_conn) = http_conn {
                if h_conn.report_incomplete(&app_data_start) {
                    return Err(ClientError::HttpFail);
                }
            }

            break;
        }
    }

    trace!("[Connection thread] Done");

    Ok(())
}

pub fn multicore_prepare_addresses(
    peer_addr: &std::net::SocketAddr, args: &ClientArgs,
) -> (
    Vec<(std::net::SocketAddr, std::net::SocketAddr)>,
    std::net::SocketAddr,
) {
    let mut tuples = vec![];
    let mut first_local_addr: Option<SocketAddr> = None;

    for src_addr in args.addrs.iter().filter(|sa| {
        (sa.is_ipv4() && peer_addr.is_ipv4())
            || (sa.is_ipv6() && peer_addr.is_ipv6())
    }) {
        tuples.push((src_addr.clone(), peer_addr.clone()));
        if first_local_addr.is_none() {
            first_local_addr = Some(*src_addr);
        }
    }

    // If there is no such address, rely on the default INADDR_IN or IN6ADDR_ANY
    // depending on the IP family of the server address. This is needed on macOS
    // and BSD variants that don't support binding to IN6ADDR_ANY for both v4
    // and v6.
    if first_local_addr.is_none() {
        let sock_addr = match peer_addr {
            std::net::SocketAddr::V4(_) => SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from_str("0.0.0.0").ok().unwrap()),
                0,
            ),
            std::net::SocketAddr::V6(_) => SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from_str("[::]").ok().unwrap()),
                0,
            ),
        };
        first_local_addr = Some(sock_addr);
    }

    (tuples, first_local_addr.unwrap())
}
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


// ! This module implements utulities in order to support multicore on top of QUICHE
// ! Author: Vany Ingenzi

use crate::args::*;
use crate::common::*;
use crate::client::*;
extern crate core_affinity;


use mio::Events;
use static_assertions;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;

use std::io::prelude::*;

use std::rc::Rc;

use std::cell::RefCell;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use mio::net::UdpSocket;
use quiche::Connection;
use quiche::ConnectionId;
use slab::Slab;
use std::sync::Arc;
use ring::rand::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

/// Reads the packets on the socket and feeds them to quiche.
pub fn read_packets_on_socket(
    socket: &UdpSocket, 
    events: &Events,
    conn: &mut Connection,
    pkt_count: &mut u64,
    dump_packet_path: &Option<String>,
    buf: &mut [u8],
) -> Result<(), ClientError>{
    let local_addr = socket.local_addr().unwrap();
    for event in events {
        match event.token() {
            _ => { // As there's only one socket listen per thread
                if event.is_readable(){
                    'read: loop {
                        let (len, from) = match socket.recv_from(buf) {
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
                
                        trace!("{}: got {} bytes", local_addr, len);
                
                        if let Some(target_path) = dump_packet_path.as_ref() {
                            let path = format!("{target_path}/{pkt_count}.pkt");
                
                            if let Ok(f) = std::fs::File::create(path) {
                                let mut f = std::io::BufWriter::new(f);
                                f.write_all(&buf[..len]).ok();
                            }
                        }
                
                        *pkt_count += 1;
                
                        let recv_info = quiche::RecvInfo {
                            to: local_addr,
                            from,
                        };
                
                        // Process potentially coalesced packets.
                        let read = match conn.recv(&mut buf[..len], recv_info) {
                            Ok(v) => v,
                
                            Err(e) => {
                                error!("{}: recv failed: {:?}", local_addr, e);
                                continue 'read;
                            },
                        };
                        debug!("{}: processed {} bytes", local_addr, read);
                    };
                }
            }
        }
    }
    Ok(())
}

pub fn write_packets_on_path(
    local_addr: &SocketAddr,
    peer_addr: &SocketAddr,
    socket: &UdpSocket, 
    conn: &mut Connection,
    out: &mut [u8],
) -> Result<(), ClientError> {
    loop {
        let (write, send_info) = match conn.send_on_path(
            out,
            Some(*local_addr),
            Some(*peer_addr),
        ) {
            Ok(v) => v,

            Err(quiche::Error::Done) => {
                trace!("{} -> {}: done writing", local_addr, peer_addr);
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

        if let Err(e) = socket.send_to(&out[..write], send_info.to) {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                trace!(
                    "{} -> {}: send() would block",
                    local_addr,
                    send_info.to
                );
                break;
            }

            return Err(ClientError::Other(format!(
                "{} -> {}: send() failed: {:?}",
                local_addr, send_info.to, e
            )));
        }

        trace!("{} -> {}: written {}", local_addr, send_info.to, write);    
    };
    Ok(())
}

fn multicore_create_socket(
    addrs: (&SocketAddr, &SocketAddr), 
    poll: &mut mio::Poll
) -> UdpSocket {
    let mut sockets = Slab::with_capacity(1);
    let (local_addr, _) = addrs;
    let socket = mio::net::UdpSocket::bind(*local_addr).unwrap();
    let token = sockets.insert(socket);
    poll.registry().register(&mut sockets[0], mio::Token(token), mio::Interest::READABLE).unwrap();
    sockets.remove(token)
}

fn multicore_initiate_connection(
    peer_addr: &std::net::SocketAddr, 
    local_addr: &std::net::SocketAddr,  
    out: &mut [u8], 
    scid: &ConnectionId, 
    conn: &mut Connection,
    socket: & mut mio::net::UdpSocket
) -> Result<(), ClientError>  {
    info!(
        "connecting to {:} from {:} with scid {:?}",
        peer_addr, local_addr, scid,
    );

    let (write, send_info) = conn.send(out).expect("initial send failed");

    while let Err(e) = socket.send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            trace!(
                "{} -> {}: send() would block",
                socket.local_addr().unwrap(),
                send_info.to
            );
            continue;
        }

        return Err(ClientError::Other(format!("send() failed: {e:?}")));
    }

    trace!("written {}", write);
    Ok(())
}

fn client_thread(
    quiche_conn: Arc<Mutex<Connection>>,
    dump_packet_path: Option<String>,
    addrs: (SocketAddr, SocketAddr),
    initiate_connection: bool,
    scid: Arc<ConnectionId>, 
    scheduler_guard: Arc<Mutex<Scheduler>>
) -> Result<(), ClientError> {
    let mut out = [0; MAX_DATAGRAM_SIZE];
    let mut pkt_count: u64 = 0;
    let mut buf = [0; 65535];
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);
    let (local_addr, peer_addr) = addrs;
    let mut socket = multicore_create_socket( (&local_addr, &peer_addr), &mut poll);
    
    if initiate_connection {
        let conn = &mut quiche_conn.lock().unwrap();
        multicore_initiate_connection(&peer_addr, &local_addr, &mut out, &scid, conn, &mut socket)?;
    }

    let mut conn_is_in_early_data ;
    let mut timeout ;

    info!("[Path Thread]: Thread {:?}, started with path {} <-> {}", thread::current().id(), local_addr, peer_addr);

    loop {
        let conn = quiche_conn.lock().unwrap();
        conn_is_in_early_data = conn.is_in_early_data(); // Check stage of the connection
        match conn.timeout() {
            Some(duration) => {
                info!("Timeout {:?}", duration);
                timeout = [Duration::from_nanos(10), duration].iter().min().cloned();
            },
            None => {
                timeout = Some(Duration::ZERO);
                info!("Received None"); 
            }
        }
        drop(conn);
        
        if !conn_is_in_early_data {
            poll.poll(&mut events, timeout).unwrap();
            if events.is_empty(){
                let conn = &mut quiche_conn.lock().unwrap();
                trace!("timed out");
                conn.on_timeout();
            }
        }
        
        let mut conn = quiche_conn.lock().unwrap();
        let mut scheduler = scheduler_guard.lock().unwrap();

        if !events.is_empty(){
            read_packets_on_socket(&socket, &events, &mut conn, &mut pkt_count, &dump_packet_path, &mut buf)?;
        }
        // Writing to socket critical section
        let (next_sender_local_addr, _) = scheduler.peek_next_path().unwrap();
        if local_addr == next_sender_local_addr{
            scheduler.next_path().unwrap(); // In order to reflect the change in the scheduler
            write_packets_on_path(&local_addr, &peer_addr, &socket, &mut conn, &mut out)?;
        }
        if conn.is_closed() {
            break; // The mutex will be drooped automatically
        }       
        drop(scheduler);
        drop(conn);
    }
    info!("[Path Thread]: Thread {:?}, Done", thread::current().id());

    Ok(())
}


pub fn multicore_connect(
    args: ClientArgs, conn_args: CommonArgs,
    output_sink: impl FnMut(String) + 'static,
) -> Result<(), ClientError> {

    static_assertions::assert_impl_all!(Connection: Send);

    // ** [Connection thread]

    let mut buf = [0; 65535];

    let output_sink = Rc::new(RefCell::new(output_sink)) as Rc<RefCell<dyn FnMut(_)>>;

    // Setup the event loop.
    // let mut poll = mio::Poll::new().unwrap();
    // let mut events = mio::Events::with_capacity(1024);

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

    let mut app_proto_selected = false;

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);
    let scid = Arc::new(scid.into_owned());
    // Create a QUIC connection and initiate handshake.
    let conn = quiche::connect(
        connect_url.domain(),
        &scid,
        local_addr,
        peer_addr,
        &mut config,
    )
    .unwrap();

    let conn_guard: Arc<Mutex<Connection>> = Arc::new(Mutex::new(conn));
    let scheduler_guard = Arc::new(Mutex::new(Scheduler::new(vec![(local_addr, peer_addr)])));

    // ! Enters critical section
    let mut conn = conn_guard.lock().unwrap();
    let core_ids = core_affinity::get_core_ids().unwrap();
    let mut current_path_thread_idx = 0;

    for (local, peer) in sockets_addrs.clone()   {
        let conn_cloned = conn_guard.clone();
        let scheduler_cloned = scheduler_guard.clone();
        // HTTPConn doesn't implement Send therefore can't be used in a Mutex
        let dump_packet_path = conn_args.dump_packet_path.clone();
        let clone_scid = scid.clone();
        let core_id = core_ids[current_path_thread_idx];
        thread::spawn(move || {
            if core_affinity::set_for_current(core_id) {
                info!("Set core affinity for {:?}", thread::current().id());
            }

            trace!("[Thread {:?}] spawned with path {:?} <-> {:?}", thread::current().id(), local, peer);
            client_thread(
                conn_cloned,
                dump_packet_path,
                (local, peer), 
                local == local_addr, 
                clone_scid, 
                scheduler_cloned
            ).unwrap();
        });
        current_path_thread_idx += 1;
    }

  
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
            conn.set_session(&session).ok();
        }
    }
    drop(conn); // ! Exits critical section

    
    // [Initial Thread Work]
    /* 
    info!(
        "connecting to {:} from {:} with scid {:?}",
        peer_addr, local_addr, scid,
    );

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");
    let token = src_addr_to_token[&send_info.from];

    while let Err(e) = sockets[token].send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            trace!(
                "{} -> {}: send() would block",
                sockets[token].local_addr().unwrap(),
                send_info.to
            );
            continue;
        }

        return Err(ClientError::Other(format!("send() failed: {e:?}")));
    }

    trace!("written {}", write);
    
    let mut pkt_count = 0;
    
    drop(conn);  */

    let mut new_path_probed = false;
    let mut probed_paths = 0;

    let app_data_start = std::time::Instant::now();

    let mut scid_sent = false;
    let mut migrated = false;

    loop {

        // ** [Per path thread]
        /* 
        if !conn.is_in_early_data() || app_proto_selected {
            poll.poll(&mut events, conn.timeout()).unwrap();
        }

        // If the event loop reported no events, it means that the timeout
        // has expired, so handle it without attempting to read packets. We
        // will then proceed with the send loop.
        if events.is_empty() {
            trace!("timed out");
            conn.on_timeout();
        } */


        // ** [Per path thread]
        /*
        // Read incoming UDP packets from the socket and feed them to quiche,
        // until there are no more packets to read.
        
        for event in &events {
            let token = event.token().into();
            let socket = &sockets[token];
            let local_addr = socket.local_addr().unwrap();
            'read: loop {
                let (len, from) = match socket.recv_from(&mut buf) {
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

                trace!("{}: got {} bytes", local_addr, len);

                if let Some(target_path) = conn_args.dump_packet_path.as_ref() {
                    let path = format!("{target_path}/{pkt_count}.pkt");

                    if let Ok(f) = std::fs::File::create(path) {
                        let mut f = std::io::BufWriter::new(f);
                        f.write_all(&buf[..len]).ok();
                    }
                }

                pkt_count += 1;

                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from,
                };

                // Process potentially coalesced packets.
                let read = match conn.recv(&mut buf[..len], recv_info) {
                    Ok(v) => v,

                    Err(e) => {
                        error!("{}: recv failed: {:?}", local_addr, e);
                        continue 'read;
                    },
                };

                trace!("{}: processed {} bytes", local_addr, read);
            }
        } */

        // ** [Connection thread]
        
        // ! Enter critical section
        let mut conn = conn_guard.lock().unwrap();  

        if conn.is_closed() {
            info!(
                "connection closed, {:?} {:?}",
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>()
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

        drop(conn); // ! Exits critical section

        // ** [Connection thread]
        // If we have an HTTP connection, first issue the requests then
        // process received data.

        // ! Enter critical section
        let mut conn = conn_guard.lock().unwrap();  

        if let Some(h_conn) = http_conn.as_mut() {
            h_conn.send_requests(&mut conn, &args.dump_response_path);
            h_conn.handle_responses(&mut conn, &mut buf, &app_data_start); // TODO reassure that the content of buf doesn't matter
        }


        drop(conn); // ! Exits critical section

        // Handle path events.
        let mut conn = conn_guard.lock().unwrap();  
        let mut conn_scheduler = scheduler_guard.lock().unwrap();
        while let Some(qe) = conn.path_event_next() {
            match qe {
                quiche::PathEvent::New(local_addr, peer_addr) => {
                    info!(
                        "[Connection Thread] Path ({}, {}) is now validated",
                        local_addr, peer_addr
                    );
                },

                quiche::PathEvent::Validated(local_addr, peer_addr) => {
                    info!(
                        "[Connection Thread] Path ({}, {}) is now validated",
                        local_addr, peer_addr
                    );
                    if conn.is_multipath_enabled() {
                        match conn.set_active(local_addr, peer_addr, true).ok() {
                            Some(()) => {
                                conn_scheduler.add_path((local_addr, peer_addr));
                            }, 
                            None => (),
                        }
                    } else if args.perform_migration {
                        conn.migrate(local_addr, peer_addr).unwrap();
                        migrated = true;
                        conn_scheduler.add_path((local_addr, peer_addr));
                    }
                },

                quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                    info!(
                        "Path ({}, {}) failed validation",
                        local_addr, peer_addr
                    );
                },

                quiche::PathEvent::Closed(local_addr, peer_addr, e, reason) => {
                    info!(
                        "Path ({}, {}) is now closed and unusable; err = {}, reason = {:?}",
                        local_addr, peer_addr, e, reason
                    );
                },

                quiche::PathEvent::ReusedSourceConnectionId(
                    cid_seq,
                    old,
                    new,
                ) => {
                    info!(
                        "Peer reused cid seq {} (initially {:?}) on {:?}",
                        cid_seq, old, new
                    );
                },

                quiche::PathEvent::PeerMigrated(..) => unreachable!(),

                quiche::PathEvent::PeerPathStatus(..) => {},
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
        
        if conn_args.multipath &&
            probed_paths < addrs.len() &&
            conn.available_dcids() > 0 &&
            conn.probe_path(addrs[probed_paths], peer_addr).is_ok()
        {
            info!("Probed path {:?}", (addrs[probed_paths], peer_addr));
            conn_scheduler.add_temporal_path((addrs[probed_paths], peer_addr));
            probed_paths += 1;
        }

        if !conn_args.multipath &&
            args.perform_migration &&
            !new_path_probed &&
            scid_sent &&
            conn.available_dcids() > 0
        {
            let additional_local_addr = sockets_addrs[1].0;
            conn.probe_path(additional_local_addr, peer_addr).unwrap();

            new_path_probed = true;
        }

        if conn.is_multipath_enabled() {
            rm_addrs.retain(|(d, addr)| {
                if app_data_start.elapsed() >= *d {
                    info!("Abandoning path {:?}", addr);
                    conn.abandon_path(
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
                    info!("Advertising path status {status:?} to {addr:?}");
                    conn.set_path_status(*addr, peer_addr, status, true)
                        .is_err()
                } else {
                    true
                }
            });
        }
        drop(conn_scheduler);
        drop(conn);
        
        // ** [Per path thread]
        /*
        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        let mut left_paths = rr_sheduler.len();
        while left_paths > 0 {
            let (local_addr, peer_addr) = rr_sheduler.next_path().unwrap();
            let token = src_addr_to_token[&local_addr];
            let socket = &sockets[token];
            loop {
                let (write, send_info) = match conn.send_on_path(
                    &mut out,
                    Some(local_addr),
                    Some(peer_addr),
                ) {
                    Ok(v) => v,

                    Err(quiche::Error::Done) => {
                        trace!("{} -> {}: done writing", local_addr, peer_addr);
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

                if let Err(e) = socket.send_to(&out[..write], send_info.to) {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!(
                            "{} -> {}: send() would block",
                            local_addr,
                            send_info.to
                        );
                        break;
                    }

                    return Err(ClientError::Other(format!(
                        "{} -> {}: send() failed: {:?}",
                        local_addr, send_info.to, e
                    )));
                }

                trace!("{} -> {}: written {}", local_addr, send_info.to, write);
            }
            left_paths -= 1;
        }*/

        // ** [Connection thread]
        let conn = conn_guard.lock().unwrap();

        if conn.is_closed() {
            info!("[Connection Thread] I saw it was over");
            info!(
                "connection closed, {:?} {:?}",
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>()
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
        drop(conn);
    }

    info!("[Connection thread] Done");

    Ok(())
}

pub fn multicore_prepare_addresses(
    peer_addr: &std::net::SocketAddr,
    args: &ClientArgs,
) -> (Vec<(std::net::SocketAddr, std::net::SocketAddr)>, std::net::SocketAddr)  {
    let mut tuples = vec![];
    let mut first_local_addr = None;

    // Create UDP sockets backing the QUIC connection, and register them with
    // the event loop. Check first user-provided addresses and keep the ones
    // compatible with the address family of the peer.
    for src_addr in args.addrs.iter().filter(|sa| {
        (sa.is_ipv4() && peer_addr.is_ipv4()) ||
            (sa.is_ipv6() && peer_addr.is_ipv6())
    }) {
        let socket = mio::net::UdpSocket::bind(*src_addr).unwrap();
        let local_addr = socket.local_addr().unwrap();
        tuples.push((src_addr.clone(), peer_addr.clone()));
        if first_local_addr.is_none(){
            first_local_addr = Some(local_addr);
        }
    }

    // If there is no such address, rely on the default INADDR_IN or IN6ADDR_ANY
    // depending on the IP family of the server address. This is needed on macOS
    // and BSD variants that don't support binding to IN6ADDR_ANY for both v4
    // and v6.
    if first_local_addr.is_none() {
        let bind_addr = match peer_addr {
            std::net::SocketAddr::V4(_) => "0.0.0.0:0",
            std::net::SocketAddr::V6(_) => "[::]:0",
        };
        let bind_addr = bind_addr.parse().unwrap();
        let socket = mio::net::UdpSocket::bind(bind_addr).unwrap(); // TODO maybe delete this line 
        let local_addr = socket.local_addr().unwrap();
        tuples.push((local_addr.clone(), peer_addr.clone()));
        first_local_addr = Some(local_addr)
    }

    (tuples, first_local_addr.unwrap())
}
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

use crate::args::*;

use std::net::ToSocketAddrs;
use crate::common::*;

use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

use mio::net::UdpSocket;
use mio::Events;
use mio::Poll;
use quiche::{MulticoreConnection, MulticorePath};
use ring::rand::*;

const MAX_DATAGRAM_SIZE: usize = 1350;
const MAX_BUF_SIZE: usize = 65507;

fn multicore_initiate_connection(
    conn_guard: &Arc<RwLock<MulticoreConnection>>, path: &mut MulticorePath,
    out: &mut [u8], socket: &mut mio::net::UdpSocket, 
) -> Result<(), ClientError> {
    info!(
        "connecting to {:} from {:}",
        path.peer_addr(), path.local_addr(),
    );

    let (write, send_info) = path.send_on_path(conn_guard, out).expect("initial send failed");
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

#[inline]
fn write_packets_on_socket(
    conn_guard: &Arc<RwLock<MulticoreConnection>>, path: &mut MulticorePath, 
    buf: &mut [u8], socket: &UdpSocket, 
) -> Result<(), ClientError> {
    let local_addr = path.local_addr();
    let peer_addr = path.peer_addr();
    loop {
        let (write, send_info) = match path.send_on_path(conn_guard, buf) {
                Ok(v) => v,

                Err(quiche::Error::Done) => {
                    //trace!("{} -> {}: done writing", local_addr, peer_addr);
                    break;
                },

                Err(e) => {
                    error!(
                        "{} -> {}: send failed: {:?}",
                        local_addr, peer_addr, e
                    );
                    let mut conn = conn_guard.write().unwrap();

                    conn.close(false, 0x1, b"fail").ok();
                    break;
                },
            };
        if let Err(e) = socket.send_to(&buf[..write], send_info.to) {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                trace!("{} -> {}: send() would block", local_addr, send_info.to);
                break;
            }

            return Err(ClientError::Other(format!(
                "{} -> {}: send() failed: {:?}",
                local_addr, send_info.to, e
            )));
        }
        trace!("{} -> {}: written {}", local_addr, send_info.to, write);
    }
    //trace!("[PARSE_EVENT] [topic: locking] [type: duration] [event: Conn + UDP Snd CS] [metadata: {:?}] [value: {:?}]", thread::current().id(), timestamp.elapsed().as_nanos());
    Ok(())
}

#[inline]
pub fn read_packets_on_socket(
    conn_guard: &Arc<RwLock<MulticoreConnection>>, path: &mut MulticorePath, 
    buf: &mut [u8], socket: &UdpSocket, events: &Events, 
) -> Result<(), ClientError> {
    let local_addr = path.local_addr();
    for event in events {
        if event.is_readable() {
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
                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from,
                };

                let read = match path.recv(conn_guard, &mut buf[..len], recv_info) {
                    Ok(v) => v,
            
                    Err(e) => {
                        error!("{}: recv failed: {:?}", local_addr, e);
                        0
                    },
                };
                trace!("{}: processed {} bytes", local_addr, read);
            }
        }
    }

    Ok(())
}


fn client_thread(
    conn_guard: Arc<RwLock<MulticoreConnection>>, 
    mut path: MulticorePath, 
    initiate_connection: bool,
    multipath_request: bool // By the conn args 
) -> Result<(), ClientError> {
    let mut out = [0; MAX_DATAGRAM_SIZE];
    let mut buf = [0; MAX_BUF_SIZE];
    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(1024);
    let local_addr = path.local_addr();
    let peer_addr = path.peer_addr();
    let mut socket = multicore_create_socket((&local_addr, &peer_addr), &mut poll);
    let rng = SystemRandom::new();

    if initiate_connection {
        multicore_initiate_connection(&conn_guard, &mut path, &mut out, &mut socket).unwrap();
    }
    
    info!(
        "[Path Thread]: Thread {:?}, started with path {} <-> {}",
        thread::current().id(),
        local_addr,
        peer_addr
    );

    let mut scid_sent = false;
    let mut probed_my_path = initiate_connection;

    loop {
        poll.poll(&mut events, Some(Duration::from_millis(10))).unwrap();
        if !events.is_empty() {
            match read_packets_on_socket(&conn_guard, &mut path, &mut buf, &socket, &events) {
                Ok(()) => {},
                Err(e) => {
                    error!(
                        "[Path Thread]: Thread {:?}, {} <-> {}", 
                        thread::current().id(),
                        local_addr,
                        peer_addr
                    );
                    return Err(e);
                }
            }
        }

        handle_path_events(&mut path);

        if initiate_connection && !scid_sent {
            let mut conn = conn_guard.write().unwrap();
            while conn.source_cids_left() > 0 {
                let (scid, reset_token) = generate_cid_and_reset_token(&rng);
    
                if conn.new_source_cid(&scid, reset_token, false).is_err() {
                    break;
                }
    
                scid_sent = true;
            }
        }

        if multipath_request &&
           !probed_my_path
        {
            let available_dcids;
            {
                let conn = conn_guard.read().unwrap();
                available_dcids = conn.available_dcids() > 0;
            }
            if  available_dcids &&
                path.probe_path(&conn_guard).is_ok() 
            {
                probed_my_path = true;
            }
        }
        
        if probed_my_path{
            write_packets_on_socket(&conn_guard, &mut path, &mut out, &socket)?;
        }

    }
}

pub fn multicore_connect(
    args: ClientArgs, conn_args: CommonArgs,
    _output_sink: impl FnMut(String) + 'static,
) -> Result<(), ClientError> {

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

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);

    // Create a QUIC connection and initiate handshake.
    let (mut conn, mut init_path) = quiche::multicore_connect(
        connect_url.domain(),
        &scid,
        local_addr,
        peer_addr,
        &mut config,
    )
    .unwrap();

    if let Some(keylog) = &mut keylog {
        if let Ok(keylog) = keylog.try_clone() {
            conn.set_keylog(Box::new(keylog));
        }
    }

    // Only bother with qlog if the user specified it.
    // TODO multicore qlog

    if let Some(session_file) = &args.session_file {
        if let Ok(session) = std::fs::read(session_file) {
            conn.set_session(&session, &mut init_path).ok();
        }
    }

    let conn_guard = Arc::new(RwLock::new(conn));
    let mut threads_join = Vec::new();
    let cloned_conn_guard = conn_guard.clone();
    let multipath_requested = conn_args.multipath;
    threads_join.push(
        thread::spawn(move || {
            client_thread(
                cloned_conn_guard, 
                init_path, 
                true, 
                multipath_requested
            )
        })
    );
    
    for (local, peer) in sockets_addrs.clone() {
        if local == local_addr {
            continue;
        }
        let path = MulticorePath::default(
            &config,
            false,
            local, 
            peer
        );
        let cloned_conn_guard = conn_guard.clone();
        let initiate_conn = local == local_addr;
        threads_join.push(
            thread::spawn(move || {
                client_thread(
                    cloned_conn_guard, 
                    path, 
                    initiate_conn, 
                    multipath_requested
                )
            })
        );
    }

    for join_handle in threads_join{
        match join_handle.join() {
            Ok(..) => {}, 
            Err(e) => {
                error!("Join return: error {:?}", e);
            }
        }
    }

    Ok(())
}

fn multicore_prepare_addresses(
    peer_addr: &std::net::SocketAddr, args: &ClientArgs,
) -> (
    Vec<(std::net::SocketAddr, std::net::SocketAddr)>,
    std::net::SocketAddr,
) {
    use std::str::FromStr;
    let mut tuples = vec![];
    let mut first_local_addr: Option<std::net::SocketAddr> = None;

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
            std::net::SocketAddr::V4(_) => std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::from_str("0.0.0.0").ok().unwrap()),
                0,
            ),
            std::net::SocketAddr::V6(_) => std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::from_str("[::]").ok().unwrap()),
                0,
            ),
        };
        first_local_addr = Some(sock_addr);
    }

    (tuples, first_local_addr.unwrap())
}

fn handle_path_events(path: &mut MulticorePath){
    while let Some(qe) = path.path_event_next() {
        match qe {
            quiche::PathEvent::New(..) => unreachable!(),

            quiche::PathEvent::Validated(local_addr, peer_addr) => {
                info!(
                    "Path ({}, {}) is now validated",
                    local_addr, peer_addr
                );
                if path.multipath {
                    path.set_active(true).ok();
                }
                // TODO multicore migration
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
}

fn multicore_create_socket(
    addrs: (&std::net::SocketAddr, &std::net::SocketAddr), poll: &mut mio::Poll,
) -> mio::net::UdpSocket {
    let (local_addr, _) = addrs;
    let mut socket = mio::net::UdpSocket::bind(*local_addr).unwrap();
    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();
    socket
}
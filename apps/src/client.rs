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
use crate::common::*;

use std::collections::HashMap;

use ring::rand::*;
use slab::Slab;
use std::io::prelude::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

pub fn connect(
    args: ClientArgs, common_args: CommonArgs,
    _output_sink: impl FnMut(String) + 'static,
) -> Result<(), ClientError> {
    let mut buf = [0; 65535];
    let mut out = [0; MAX_DATAGRAM_SIZE];

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Resolve server address.
    let peer_addr = if let Some(addr) = &args.connect_to {
        addr.parse().expect("--connect-to is expected to be a string containing an IPv4 or IPv6 address with a port. E.g. 192.0.2.0:443")
    } else {
        return Err(ClientError::Other("Can't find the address to connect to".into()));
    };

    let (sockets, src_addr_to_token, local_addr, token_to_peer_addr) =
        create_sockets(&mut poll, &peer_addr, &args, &common_args);
    let mut addrs = Vec::with_capacity(sockets.len());
    addrs.push(local_addr);
    for src in src_addr_to_token.keys() {
        if *src != local_addr {
            addrs.push(*src);
        }
    }

    // Warn the user if there are more usable addresses than the advertised
    // `active_connection_id_limit`.
    if addrs.len() as u64 > common_args.max_active_cids {
        warn!(
            "{} addresses provided, but configuration restricts to at most {} \
               active CIDs; increase the --max-active-cids parameter to use all \
               the provided addresses",
            addrs.len(),
            common_args.max_active_cids
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

    config.set_application_protos(&common_args.alpns).unwrap();

    config.set_max_idle_timeout(common_args.idle_timeout);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(common_args.max_data);
    config.set_initial_max_stream_data_bidi_local(common_args.max_stream_data);
    config.set_initial_max_stream_data_bidi_remote(common_args.max_stream_data);
    config.set_initial_max_stream_data_uni(common_args.max_stream_data);
    config.set_initial_max_streams_bidi(common_args.max_streams_bidi);
    config.set_initial_max_streams_uni(common_args.max_streams_uni);
    config.set_disable_active_migration(!common_args.enable_active_migration);
    config.set_active_connection_id_limit(common_args.max_active_cids);
    config.set_multipath(common_args.multipath);

    config.set_max_connection_window(common_args.max_window);
    config.set_max_stream_window(common_args.max_stream_window);

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

    if common_args.no_grease {
        config.grease(false);
    }

    if common_args.early_data {
        config.enable_early_data();
    }

    config
        .set_cc_algorithm_name(&common_args.cc_algorithm)
        .unwrap();

    if common_args.disable_hystart {
        config.enable_hystart(false);
    }

    if common_args.dgrams_enabled {
        config.enable_dgram(true, 1000, 1000);
    }

    let mut app_conn: Option<AdaptedMcMPQUICConnClient> = None;

    let mut app_proto_selected = false;

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);

    // Create a QUIC connection and initiate handshake.
    let mut conn = quiche::connect(
        Some("quic.tech"),
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

    let app_data_start = std::time::Instant::now();

    let mut probed_paths = 0;
    let mut pkt_count = 0;

    let mut scid_sent = false;
    let mut new_path_probed = false;
    let mut migrated = false;

    loop {
        if !conn.is_in_early_data() || app_proto_selected {
            poll.poll(&mut events, conn.timeout()).unwrap();
        }

        // If the event loop reported no events, it means that the timeout
        // has expired, so handle it without attempting to read packets. We
        // will then proceed with the send loop.
        if events.is_empty() {
            trace!("timed out");

            conn.on_timeout();
        }

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

                if let Some(target_path) = common_args.dump_packet_path.as_ref() {
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
        }

        trace!("done reading");

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

            if let Some(h_conn) = app_conn {
                if h_conn.report_incomplete(&app_data_start) {
                    return Err(ClientError::HttpFail);
                }
            }

            break;
        }

        // Create a new application protocol session once the QUIC connection is
        // established.
        if (conn.is_established() || conn.is_in_early_data())
            && (!args.perform_migration || migrated)
            && !app_proto_selected
        {
            // At this stage the ALPN negotiation succeeded and selected a
            // single application protocol name. We'll use this to construct
            // the correct type of HttpConn but `application_proto()`
            // returns a slice, so we have to convert it to a str in order
            // to compare to our lists of protocols. We `unwrap()` because
            // we need the value and if something fails at this stage, there
            // is not much anyone can do to recover.

            let app_proto = conn.application_proto();

            if alpns::MMPQUIC.contains(&app_proto) {
                app_conn = Some(AdaptedMcMPQUICConnClient::new(sockets.len()));
                app_proto_selected = true;
                info!("proto selected");
            } else {
                error!("App protocol not mcMPQUIC !");
                match conn.close(true, 0, b"APLN not mMPQUIC".as_slice()) {
                    // Already closed.
                    Ok(_) | Err(quiche::Error::Done) => (),

                    Err(e) => panic!("error closing conn: {:?}", e),
                }
            }
        }

        // If we have an HTTP connection, first issue the requests then
        // process received data.
        if let Some(h_conn) = app_conn.as_mut() {
            if h_conn.recv(&mut conn, &mut buf) {
                info!(
                    "receieved in {:?} app_conn: {:?}",
                    app_data_start.elapsed(),
                    h_conn
                );
                match conn.close(true, 0, b"") {
                    // Already closed.
                    Ok(_) | Err(quiche::Error::Done) => (),

                    Err(e) => panic!("error closing conn: {:?}", e),
                }
            }
        }

        // Handle path events.
        while let Some(qe) = conn.path_event_next() {
            match qe {
                quiche::PathEvent::New(..) => unreachable!(),

                quiche::PathEvent::Validated(local_addr, peer_addr) => {
                    info!(
                        "Path ({}, {}) is now validated",
                        local_addr, peer_addr
                    );
                    if conn.is_multipath_enabled() {
                        conn.set_active(local_addr, peer_addr, true).ok();
                    } else if args.perform_migration {
                        conn.migrate(local_addr, peer_addr).unwrap();
                        migrated = true;
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

        if common_args.multipath
            && probed_paths < addrs.len()
            && conn.available_dcids() > 0
        {
            let addr_to_probe = addrs[probed_paths];
            let token = src_addr_to_token.get(&addr_to_probe).unwrap();
            let peer_addr_to_probe = token_to_peer_addr.get(&token).unwrap();
            if conn.probe_path(addrs[probed_paths], *peer_addr_to_probe).is_ok() {
                probed_paths += 1;
            }
        }

        if !common_args.multipath
            && args.perform_migration
            && !new_path_probed
            && scid_sent
            && conn.available_dcids() > 0
        {
            let additional_local_addr = sockets[1].local_addr().unwrap();
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

        // Determine in which order we are going to iterate over paths.
        let scheduled_tuples = largest_cwnd_scheduler(&conn);

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        for (local_addr, peer_addr) in scheduled_tuples {
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
        }

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

            if let Some(h_conn) = app_conn {
                if h_conn.report_incomplete(&app_data_start) {
                    return Err(ClientError::HttpFail);
                }
            }

            break;
        }
    }

    Ok(())
}

fn create_sockets(
    poll: &mut mio::Poll, connect_to_addr: &std::net::SocketAddr, args: &ClientArgs, common_args: &CommonArgs
) -> (
    Slab<mio::net::UdpSocket>,
    HashMap<std::net::SocketAddr, usize>,
    std::net::SocketAddr,
    HashMap<usize, std::net::SocketAddr>,
) {
    let mut sockets = Slab::with_capacity(std::cmp::max(args.addrs.len(), 1));
    let mut src_addrs = HashMap::new();
    let mut first_local_addr = None;
    let mut local_addrs = vec![];
    let mut peer_addrs = vec![connect_to_addr.clone()];
    let mut peer_addrs_map = HashMap::new();

    if args.addrs.len() != common_args.server_addresses.len() + 1 { // Removing the --connect-to address
        info!("client addresses: {:?}", args.addrs.len());
        info!("server addresses: {:?} + connect-to address: {:?}", common_args.server_addresses, connect_to_addr);
        panic!("The client addresses don't much the server's addresses.");
    }

    for src_addr in args.addrs.iter().filter(|sa| {
        (sa.is_ipv4() && connect_to_addr.is_ipv4())
            || (sa.is_ipv6() && connect_to_addr.is_ipv6())
    }) {
        local_addrs.push(src_addr.clone());
        if first_local_addr.is_none() {
            first_local_addr = Some(*src_addr);
        }
    }

    let local_addr = &local_addrs[0]; // There must be a local address for our testbed

    for dst_addr in common_args
        .server_addresses
        .iter().filter(|sa| {
            (sa.is_ipv4() && local_addr.is_ipv4())
                || (sa.is_ipv6() && local_addr.is_ipv6()) })
    {
        if dst_addr == connect_to_addr{
            continue;
        }
        peer_addrs.push(dst_addr.clone());
    }

    for (i, src_addr) in local_addrs.iter().enumerate() {
        let socket = mio::net::UdpSocket::bind(*src_addr).unwrap();
        let local_addr = socket.local_addr().unwrap();
        let token = sockets.insert(socket);
        src_addrs.insert(local_addr, token);
        let peer_addr = peer_addrs[i];
        peer_addrs_map.insert(token, peer_addr);
        poll.registry()
            .register(
                &mut sockets[token],
                mio::Token(token),
                mio::Interest::READABLE,
            )
            .unwrap();
        if first_local_addr.is_none() {
            first_local_addr = Some(local_addr);
        }
    }

    (sockets, src_addrs, first_local_addr.unwrap(), peer_addrs_map)
}

fn largest_cwnd_scheduler(
    conn: &quiche::Connection,
) -> impl Iterator<Item = (std::net::SocketAddr, std::net::SocketAddr)> {
    use itertools::Itertools;
    conn.path_stats()
        .filter(|p| !matches!(p.state, quiche::PathState::Closed(_, _)))
        .sorted_by_key(|p| p.rtt)
        .map(|p| (p.local_addr, p.peer_addr))
}

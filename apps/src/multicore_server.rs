use quiche::{Config, MulticoreConnection, MulticorePath};
use ring::rand::*;
use std::convert::TryFrom;
use std::fs::File;
use std::io;
use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

use crate::args::*;
use crate::common::*;
use crate::sendto::*;

use mio::{Interest, Token};
use std::sync::mpsc::{channel, Receiver, Sender};

const MAX_BUF_SIZE: usize = 65507;
const MAX_DATAGRAM_SIZE: usize = 1350;

fn server_thread(
    client: Arc<MulticoreClient>, mut path: MulticorePath, args: ServerArgs,
    nb_initiated_paths: usize,
    mut tx_finish_channel: Option<Sender<MulticorePathDoneInfo>>,
    mut rx_finish_channel: Option<Receiver<MulticorePathDoneInfo>>,
) -> () {
    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];

    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    let local_addr = path.local_addr();
    let mut peer_addr = path.peer_addr();
    let (socket, pacing, enable_gso) = multicore_create_socket(
        &local_addr,
        &mut poll,
        args.disable_pacing,
        args.disable_gso,
    );

    debug!(
        "[Server Thread] Thread ID : {:?} Started with path {:?}",
        thread::current().id(),
        local_addr
    );

    // All the followings are one time flags. They switch from false to true
    let mut continue_write = false;
    let mut loss_rate;
    let max_datagram_size = MAX_DATAGRAM_SIZE;
    let mut max_send_burst = MAX_BUF_SIZE;

    let mut created_app_conn = false;
    let mut is_established = false;
    let mut is_in_early_data = false;

    let mut mmpquic_conn = None;
    let mut nb_active_paths = nb_initiated_paths;
    let mut can_close_conn = false;
    let mut scid_sent = false;
    let mut has_set_peer_addr = rx_finish_channel.is_some();
    let rng = SystemRandom::new();
    let mut probed_my_path = rx_finish_channel.is_some();

    let path_transfer_size = if let Some(s) = args.transfer_size {
        Some(s / nb_initiated_paths)
    } else {
        None
    };

    loop {
        let timeout = match continue_write {
            true => Some(Duration::ZERO),
            false => Some(Duration::from_millis(10)), // TODO multicore timeout
        };

        poll.poll(&mut events, timeout).unwrap();

        'read: loop {
            if events.is_empty() && !continue_write {
                trace!("timed out");

                // TODO multicore timeout
                path.on_timeout(&client.conn);
                break 'read;
            }

            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read, so end the read
                    // loop.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("[Path Thread] recv() would block");
                        break 'read;
                    }

                    panic!("[Path Thread] recv() failed: {:?}", e);
                },
            };

            trace!("[Path Thread] got {} bytes", len);

            let recv_info = quiche::RecvInfo {
                to: local_addr,
                from,
            };

            if !has_set_peer_addr {
                path.set_peer_addr(from).unwrap();
                has_set_peer_addr = true;
                peer_addr = path.peer_addr();
            }

            // Process potentially coalesced packets.
            let read = match path.recv(&client.conn, &mut buf[..len], recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("[Path Thread] recv failed: {:?}", e);
                    continue 'read;
                },
            };

            trace!("[Path Thread] processed {} bytes", read);

            if rx_finish_channel.is_some() && !scid_sent {
                // TODO multicore retired scid
                // Provides as many CIDs as possible.
                while path.source_cids_left(&client.conn) > 0 {
                    let (scid, reset_token) = generate_cid_and_reset_token(&rng);
                    if path
                        .new_source_cid(&client.conn, &scid, reset_token, false)
                        .is_err()
                    {
                        break;
                    }
                    scid_sent = true;
                }
            }
        }

        if rx_finish_channel.is_some() {
            handle_done_paths(
                rx_finish_channel.as_mut(),
                &mut nb_active_paths,
            )
            .unwrap();
        }

        if !created_app_conn {
            if (is_in_early_data || is_established) && path.is_active() {
                let app_proto;
                {
                    let conn = client.conn.read().unwrap();
                    app_proto = conn.application_proto().to_vec();
                }

                #[allow(clippy::box_default)]
                if alpns::MMPQUIC.contains(&app_proto.as_slice()) {
                    let stream_id;
                    {
                        let mut next_stream_id =
                            client.next_stream_id.write().unwrap();
                        stream_id = next_stream_id.clone();
                        *next_stream_id += 4; // We are using server unidirectional streams
                    }
                    mmpquic_conn = Some(McMPQUICConnServer::new(
                        &mut path,
                        &client.conn,
                        stream_id,
                        path_transfer_size,
                        args.transfer_time,
                    ));
                    created_app_conn = true;
                    info!("{:?} <-> {:?} created app conn", path.local_addr(), path.peer_addr());
                } else {
                    error!("APLN not mMPQUIC");
                    path.close_connection(
                        &client.conn,
                        true,
                        0,
                        b"APLN not mMPQUIC".as_slice(),
                    )
                    .unwrap();
                }
            } else {
                let conn = client.conn.read().unwrap();
                is_in_early_data = conn.is_in_early_data();
                is_established = conn.is_established();
            }
        }

        if created_app_conn && !can_close_conn {
            if let Some(app_conn) = mmpquic_conn.as_mut() {
                let fin = app_conn.send(&mut path, &client.conn, &mut buf);
                if fin {
                    info!(
                        "path {:?} <-> {:?} done sending: app_conn: {:?}",
                        local_addr, peer_addr, app_conn
                    );
                    can_close_conn = true;
                }
            }
        }

        if can_close_conn {
            if path.no_data_inflight() && tx_finish_channel.is_some() {
                info!("path {:?} <-> {:?} finished", local_addr, peer_addr);
                if signal_path_done(
                    &mut path,
                    MulticoreStatus::Success,
                    tx_finish_channel.as_mut(),
                ) {
                    return;
                }
            } else {
                let conn = client.conn.read().unwrap();
                if conn.recv_application_close || conn.recv_connection_close {
                    if signal_path_done(
                        &mut path,
                        MulticoreStatus::Success,
                        tx_finish_channel.as_mut(),
                    ) {
                        info!(
                            "path {:?} <-> {:?} finished, recv close from peer",
                            local_addr, peer_addr
                        );
                        return;
                    }
                }
            }
        }

        path.handle_pending_multicore_events().unwrap();
        handle_path_events(&mut path, &client.conn, &mut probed_my_path);

        continue_write = false;
        loss_rate = if path.has_inner_path() {
            path.stats().lost as f64 / path.stats().sent as f64
        } else {
            0.0 as f64
        };

        if loss_rate > loss_rate + 0.001 {
            max_send_burst = max_send_burst / 4 * 3;
            // Minimun bound of 10xMSS.
            max_send_burst = max_send_burst.max(max_datagram_size * 10);
        }

        let max_send_burst = path.send_quantum().min(max_send_burst)
            / max_datagram_size
            * max_datagram_size;

        let mut total_write = 0;
        let mut dst_info: Option<quiche::SendInfo> = None;

        while total_write < max_send_burst && probed_my_path {
            let res = path.send_on_path(
                &client.conn,
                &mut out[total_write..max_send_burst],
            );

            let (write, send_info) = match res {
                Ok(v) => v,

                Err(quiche::Error::Done) => {
                    trace!(
                        "{} <-> {} done writing",
                        path.local_addr(),
                        path.peer_addr()
                    );
                    break;
                },

                Err(quiche::Error::MulticoreDrainingConn) => {
                    if signal_path_done(
                        &mut path,
                        if can_close_conn {
                            MulticoreStatus::Success
                        } else {
                            MulticoreStatus::Error
                        },
                        tx_finish_channel.as_mut(),
                    ) {
                        debug!(
                            "path {:?} <-> {:?} finished, send connection draining",
                            local_addr, peer_addr
                        );
                        return;
                    }
                    can_close_conn = true;
                    break;
                },

                Err(e) => {
                    error!(
                        "{} <-> {} send failed: {:?}",
                        path.local_addr(),
                        path.peer_addr(),
                        e
                    );
                    path.close_connection(&client.conn, false, 0x1, b"fail")
                        .ok();
                    break;
                },
            };

            total_write += write;

            let _ = dst_info.get_or_insert(send_info);

            if write < max_datagram_size {
                continue_write = true;
                break;
            }
        }

        if !(total_write == 0 || dst_info.is_none()) {
            if let Err(e) = send_to(
                &socket,
                &out[..total_write],
                &dst_info.unwrap(),
                max_datagram_size,
                pacing,
                enable_gso,
            ) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    trace!("send() would block");
                } else {
                    panic!("send_to() failed: {:?}", e);
                }
            }

            trace!("written {} bytes", total_write);

            if continue_write {
                trace!("pause writing");
            } else if total_write >= max_send_burst {
                trace!("pause writing");
                continue_write = true;
            }
        }

        if can_close_conn && nb_active_paths == 1 {
            let conn = client.conn.read().unwrap();
            if conn.is_closed() {
                info!("connection collected {:?}", conn.stats(),);
                return;
            }
        }
    }
}

pub fn multicore_start_server(args: ServerArgs, common_args: CommonArgs) {
    env_logger::builder()
        .default_format_timestamp_nanos(true)
        .init();

    let mut pacing = false;

    // Dumy socket used for detecting gso
    let socket =
        mio::net::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();

    // Set SO_TXTIME socket option on the listening UDP socket for pacing
    // outgoing packets.
    if !args.disable_pacing {
        match set_txtime_sockopt(&socket) {
            Ok(_) => {
                pacing = true;
                debug!("successfully set SO_TXTIME socket option");
            },
            Err(e) => debug!("setsockopt failed {:?}", e),
        };
    }

    let max_datagram_size = MAX_DATAGRAM_SIZE;
    let enable_gso = if args.disable_gso {
        false
    } else {
        detect_gso(&socket, max_datagram_size)
    };

    trace!("GSO detected: {}", enable_gso);

    // Create the configuration for the QUIC connections.
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();

    config.load_cert_chain_from_pem_file(&args.cert).unwrap();
    config.load_priv_key_from_pem_file(&args.key).unwrap();

    config.set_application_protos(&common_args.alpns).unwrap();

    config.set_max_idle_timeout(common_args.idle_timeout);
    config.set_max_recv_udp_payload_size(max_datagram_size);
    config.set_max_send_udp_payload_size(max_datagram_size);
    config.set_initial_max_data(common_args.max_data);
    config.set_initial_max_stream_data_bidi_local(common_args.max_stream_data);
    config.set_initial_max_stream_data_bidi_remote(common_args.max_stream_data);
    config.set_initial_max_stream_data_uni(common_args.max_stream_data);
    config.set_initial_max_streams_bidi(common_args.max_streams_bidi);
    config.set_initial_max_streams_uni(common_args.max_streams_uni);
    config.set_disable_active_migration(!common_args.enable_active_migration);
    config.set_active_connection_id_limit(common_args.max_active_cids);
    config.set_initial_congestion_window_packets(
        usize::try_from(common_args.initial_cwnd_packets).unwrap(),
    );
    config.set_multipath(common_args.multipath);

    config.set_max_connection_window(common_args.max_window);
    config.set_max_stream_window(common_args.max_stream_window);

    config.enable_pacing(pacing);

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

    if common_args.early_data {
        config.enable_early_data();
    }

    if common_args.no_grease {
        config.grease(false);
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

    let local_addr = args.listen.parse().unwrap();

    drop(socket); // Garbage collect the initial socket

    loop {
        let (conn, init_path) =
            wait_for_new_connection(&mut config, &args, &mut keylog, local_addr);

        let client = MulticoreClient {
            conn,
            next_stream_id: Arc::new(RwLock::new(3)),
        };
        let client = Arc::new(client);
        let copied_args = args.clone();
        let (tx, rx) = channel();
        let nb_initiated_paths = common_args.server_addresses.len() + 1;
        let clone_client_arc = client.clone();
        let core_ids = common_args.cpu_aff_cores.clone().unwrap_or(Vec::new());
        let peer_addr = init_path.peer_addr();
        let set_core_affinity = common_args.cpu_aff_cores.is_some();

        let mut joins = Vec::new();
        let core_id = if set_core_affinity { Some(core_ids[0]) } else { None };
        joins.push(thread::spawn(move || {
            if set_core_affinity {
                if core_affinity::set_for_current(core_id.unwrap()) {
                    debug!("set core affinity to {:?} for {:?}", core_id.unwrap(), thread::current().id());
                }
            }
            server_thread(
                clone_client_arc,
                init_path,
                copied_args,
                nb_initiated_paths,
                None,
                Some(rx),
            )
        }));
        let mut current_core_id = 1;

        for local in common_args.server_addresses.iter() {
            if *local == local_addr {
                continue;
            }
            let additional_path =
                MulticorePath::default(&config, true, *local, peer_addr);
            let copied_args = args.clone();
            let cloned_client = client.clone();
            let tx_clone = tx.clone();
            let core_id = if set_core_affinity { Some(core_ids[current_core_id]) } else { None };
            current_core_id = if set_core_affinity { current_core_id+1 } else { current_core_id };
            joins.push(thread::spawn(move || {
                if set_core_affinity {
                    if core_affinity::set_for_current(core_id.unwrap()) {
                        debug!(
                            "set core affinity to {:?} for {:?}",
                            core_id.unwrap(),
                            thread::current().id()
                        );
                    }
                }
                server_thread(
                    cloned_client,
                    additional_path,
                    copied_args,
                    nb_initiated_paths,
                    Some(tx_clone),
                    None,
                )
            }));
        }

        for join_handle in joins {
            match join_handle.join() {
                Ok(..) => {},
                Err(e) => {
                    error!("join return: error {:?}", e);
                },
            }
        }
    }
}

fn wait_for_new_connection(
    config: &mut Config, args: &ServerArgs, keylog: &mut Option<File>,
    local_addr: std::net::SocketAddr,
) -> (Arc<RwLock<MulticoreConnection>>, MulticorePath) {
    let rng = SystemRandom::new();

    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);
    let mut socket = mio::net::UdpSocket::bind(local_addr).unwrap();

    let conn_id_seed =
        ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &rng).unwrap();
    poll.registry()
        .register(&mut socket, Token(0), Interest::READABLE)
        .unwrap();
    loop {
        info!("[Listener] listening on {:}", socket.local_addr().unwrap());

        poll.poll(&mut events, None).unwrap();

        'read: loop {
            // If the event loop reported no events, it means that the timeout
            // has expired, so handle it without attempting to read packets. We
            // will then proceed with the send loop.
            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read, so end the read
                    // loop.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("[Listener] recv() would block");
                        break 'read;
                    }

                    panic!("[Listener] recv() failed: {:?}", e);
                },
            };

            trace!("[Listener] got {} bytes", len);

            let pkt_buf = &mut buf[..len];

            // TODO multicore Add packet dump

            // Parse the QUIC packet's header.
            let hdr = match quiche::Header::from_slice(
                pkt_buf,
                quiche::MAX_CONN_ID_LEN,
            ) {
                Ok(v) => v,

                Err(e) => {
                    error!("[Listener] Parsing packet header failed: {:?}", e);
                    continue 'read;
                },
            };

            trace!("[Listener] got packet {:?}", hdr);

            let pkt_buf = &mut buf[..len];

            // Parse the QUIC packet's header.
            let hdr = match quiche::Header::from_slice(
                pkt_buf,
                quiche::MAX_CONN_ID_LEN,
            ) {
                Ok(v) => v,

                Err(e) => {
                    error!("[Listener] Parsing packet header failed: {:?}", e);
                    continue 'read;
                },
            };

            trace!("[Listener] got packet {:?}", hdr);
            let conn_id = ring::hmac::sign(&conn_id_seed, &hdr.dcid);
            let conn_id = &conn_id.as_ref()[..quiche::MAX_CONN_ID_LEN];

            if hdr.ty != quiche::Type::Initial {
                error!("Packet is not Initial");
                continue 'read;
            }

            if !quiche::version_is_supported(hdr.version) {
                warn!("Doing version negotiation");

                let len =
                    quiche::negotiate_version(&hdr.scid, &hdr.dcid, &mut out)
                        .unwrap();

                let out = &out[..len];

                if let Err(e) = socket.send_to(out, from) {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("send() would block");
                        break;
                    }

                    panic!("send() failed: {:?}", e);
                }
                continue 'read;
            }

            let mut scid = [0; quiche::MAX_CONN_ID_LEN];
            scid.copy_from_slice(&conn_id);

            let mut odcid = None;

            if !args.no_retry {
                // Token is always present in Initial packets.
                let token = hdr.token.as_ref().unwrap();

                // Do stateless retry if the client didn't send a token.
                if token.is_empty() {
                    warn!("Doing stateless retry");

                    let scid = quiche::ConnectionId::from_ref(&scid);
                    let new_token = mint_token(&hdr, &from);

                    let len = quiche::retry(
                        &hdr.scid,
                        &hdr.dcid,
                        &scid,
                        &new_token,
                        hdr.version,
                        &mut out,
                    )
                    .unwrap();

                    let out = &out[..len];

                    if let Err(e) = socket.send_to(out, from) {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            trace!("send() would block");
                            break;
                        }

                        panic!("send() failed: {:?}", e);
                    }
                    continue 'read;
                }

                odcid = validate_token(&from, token);

                // The token was not valid, meaning the retry failed, so
                // drop the packet.
                if odcid.is_none() {
                    error!("Invalid address validation token");
                    continue;
                }

                if scid.len() != hdr.dcid.len() {
                    error!("Invalid destination connection ID");
                    continue 'read;
                }

                // Reuse the source connection ID we sent in the Retry
                // packet, instead of changing it again.
                scid.copy_from_slice(&hdr.dcid);
            }

            let scid = quiche::ConnectionId::from_vec(scid.to_vec());

            debug!("New connection: dcid={:?} scid={:?}", hdr.dcid, scid);

            let (mut conn, mut init_path) = quiche::multicore_accept(
                &scid,
                odcid.as_ref(),
                local_addr,
                from,
                config,
            )
            .unwrap();

            if let Some(keylog) = keylog.as_mut() {
                if let Ok(keylog) = keylog.try_clone() {
                    conn.set_keylog(Box::new(keylog));
                }
            }

            let conn_guard = Arc::new(RwLock::new(conn));

            let recv_info = quiche::RecvInfo {
                to: local_addr,
                from,
            };

            let read = match init_path.recv(&conn_guard, pkt_buf, recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("[Path Thread] recv failed: {:?}", e);
                    continue 'read;
                },
            };

            trace!("[Listener] processed {} bytes", read);
            return (conn_guard, init_path);
        }
    }
}

fn multicore_create_socket(
    local_addr: &std::net::SocketAddr, poll: &mut mio::Poll,
    disable_pacing: bool, disable_gso: bool,
) -> (mio::net::UdpSocket, bool, bool) {
    let mut socket = mio::net::UdpSocket::bind(*local_addr).unwrap();
    let mut pacing = false;

    if !disable_pacing {
        match set_txtime_sockopt(&socket) {
            Ok(_) => {
                pacing = true;
                debug!("successfully set SO_TXTIME socket option");
            },
            Err(e) => debug!("setsockopt failed {:?}", e),
        }
    }

    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();

    let max_datagram_size = MAX_DATAGRAM_SIZE;
    let enable_gso = if disable_gso {
        false
    } else {
        detect_gso(&socket, max_datagram_size)
    };

    trace!("GSO detected: {}", enable_gso);

    (socket, pacing, enable_gso)
}

/// Generate a stateless retry token.
///
/// The token includes the static string `"quiche"` followed by the IP address
/// of the client and by the original destination connection ID generated by the
/// client.
///
/// Note that this function is only an example and doesn't do any cryptographic
/// authenticate of the token. *It should not be used in production system*.
fn mint_token(hdr: &quiche::Header, src: &std::net::SocketAddr) -> Vec<u8> {
    let mut token = Vec::new();

    token.extend_from_slice(b"quiche");

    let addr = match src.ip() {
        std::net::IpAddr::V4(a) => a.octets().to_vec(),
        std::net::IpAddr::V6(a) => a.octets().to_vec(),
    };

    token.extend_from_slice(&addr);
    token.extend_from_slice(&hdr.dcid);

    token
}

/// Validates a stateless retry token.
///
/// This checks that the ticket includes the `"quiche"` static string, and that
/// the client IP address matches the address stored in the ticket.
///
/// Note that this function is only an example and doesn't do any cryptographic
/// authenticate of the token. *It should not be used in production system*.
fn validate_token<'a>(
    src: &std::net::SocketAddr, token: &'a [u8],
) -> Option<quiche::ConnectionId<'a>> {
    if token.len() < 6 {
        return None;
    }

    if &token[..6] != b"quiche" {
        return None;
    }

    let token = &token[6..];
    let addr = match src.ip() {
        std::net::IpAddr::V4(a) => a.octets().to_vec(),
        std::net::IpAddr::V6(a) => a.octets().to_vec(),
    };

    if token.len() < addr.len() || &token[..addr.len()] != addr.as_slice() {
        return None;
    }
    Some(quiche::ConnectionId::from_ref(&token[addr.len()..]))
}

/// Set SO_TXTIME socket option.
///
/// This socket option is set to send to kernel the outgoing UDP
/// packet transmission time in the sendmsg syscall.
///
/// Note that this socket option is set only on linux platforms.
#[cfg(target_os = "linux")]
fn set_txtime_sockopt(sock: &mio::net::UdpSocket) -> io::Result<()> {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::TxTime;
    use std::os::unix::io::AsRawFd;

    let config = nix::libc::sock_txtime {
        clockid: libc::CLOCK_MONOTONIC,
        flags: 0,
    };

    // mio::net::UdpSocket doesn't implement AsFd (yet?).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(sock.as_raw_fd()) };

    setsockopt(&fd, TxTime, &config)?;

    Ok(())
}

fn handle_path_events(
    path: &mut MulticorePath, conn_guard: &Arc<RwLock<MulticoreConnection>>,
    probed_my_path: &mut bool,
) {
    while let Some(qe) = path.path_event_next() {
        match qe {
            quiche::PathEvent::New(local_addr, peer_addr) => {
                info!("Seen new path ({}, {})", local_addr, peer_addr);
                // Directly probe the new path.
                match path.probe_path(conn_guard) {
                    Ok(..) => *probed_my_path = true,
                    Err(e) => {
                        error!("cannot probe: {}", e)
                    },
                }
            },

            quiche::PathEvent::Validated(local_addr, peer_addr) => {
                info!("Path ({}, {}) is now validated", local_addr, peer_addr);
                if path.multipath {
                    path.set_active(true)
                        .map_err(|e| error!("cannot set path active: {}", e))
                        .ok();
                } else {
                    warn!("Can't set path active not multipath");
                }
            },

            quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                error!("Path ({}, {}) failed validation", local_addr, peer_addr);
            },

            quiche::PathEvent::Closed(local_addr, peer_addr, err, reason) => {
                info!(
                    "Path ({}, {}) is now closed and unusable; err = {} reason = {:?}",
                    local_addr,
                    peer_addr,
                    err,
                    reason,
                );
            },

            quiche::PathEvent::ReusedSourceConnectionId(cid_seq, old, new) => {
                info!(
                    "Peer reused cid seq {} (initially {:?}) on {:?}",
                    cid_seq, old, new
                );
            },

            quiche::PathEvent::PeerMigrated(local_addr, peer_addr) => {
                info!("Connection migrated to ({}, {})", local_addr, peer_addr);
            },

            quiche::PathEvent::PeerPathStatus(addr, path_status) => {
                info!("Peer asks status {:?} for {:?}", path_status, addr,);
                info!("TODO multicore");
                // TODO multicore
            },
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn set_txtime_sockopt(_: &mio::net::UdpSocket) -> io::Result<()> {
    use std::io::Error;
    use std::io::ErrorKind;

    Err(Error::new(
        ErrorKind::Other,
        "Not supported on this platform",
    ))
}

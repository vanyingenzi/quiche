use std::fs::File;
use std::io;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::thread;
use quiche::Config;
use ring::rand::*;
use std::sync::Arc;

use mio::{Events, Poll, Token, Waker, Interest};
use spin::Mutex;
use crate::args::*;
use crate::common::*;
use crate::sendto::*;

use std::time::{Duration, Instant};

const MAX_BUF_SIZE: usize = 65507;
const MAX_DATAGRAM_SIZE: usize = 1350;

pub fn server_thread(
    client_guard: Arc<MulticoreClient>, 
    addr: (std::net::SocketAddr, std::net::SocketAddr)
) {
    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];
    let mut pacing = false;

    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    let (local, _) = addr;

    let mut socket = mio::net::UdpSocket::bind(local).unwrap();
    match set_so_reuseport_sockopt(&socket){
        Ok (_) => {
            debug!("Successfully set SO_REUSEPORT socket option");
        }, 
        Err(e) => {
            error!("set_so_reuseport_sockopt failed: {:?}", e);
        }
    }

    /*if !client_guard.args.disable_pacing {
        match set_txtime_sockopt(&socket) {
            Ok(_) => {
                pacing = true;
                debug!("successfully set SO_TXTIME socket option");
            },
            Err(e) => debug!("setsockopt failed {:?}", e),
        };
    }*/

    trace!("[PATH_THREAD] Thred: {:?} listening on port", thread::current().id());

    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();

    let max_datagram_size = MAX_DATAGRAM_SIZE;
    /*let enable_gso = if client_guard.args.disable_gso {
        false
    } else {
        detect_gso(&socket, max_datagram_size)
    };*/

    let mut continue_write = false;
    // trace!("GSO detected: {}", enable_gso);

    'main: loop {
        let timeout = match continue_write {
            true => Some(Duration::ZERO),
            false => {
                let conn = client_guard.conn.lock();
                let mut conn_paths = client_guard.conn_paths.lock();
                conn.timeout(&mut conn_paths)
            }
        };

        poll.poll(&mut events, timeout).unwrap();

        /*
        'read: loop{
            if events.is_empty() && !continue_write{
                trace!("timed out");
                let mut conn = client_guard.conn.lock();
                let mut conn_paths = client_guard.conn_paths.lock();
                conn.on_timeout(&mut conn_paths);
                break 'read;
            }

            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read, so end the read
                    // loop.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("recv() would block");
                        break 'read;
                    }

                    panic!("recv() failed: {:?}", e);
                },
            };

            trace!("got {} bytes", len);

            let pkt_buf = &mut buf[..len];

            // TODO add packet dumping
            let recv_info = quiche::RecvInfo {
                to: local,
                from,
            };

            let mut conn = client_guard.conn.lock();
            let mut conn_paths = client_guard.conn_paths.lock();
            let mut http_conn = client_guard.http_conn.lock();
            let mut app_proto_selected = client_guard.app_proto_selected.lock();
            let conn_args = client_guard.conn_args;
            let mut max_datagram_size = client_guard.max_datagram_size.lock();

            // Process potentially coalesced packets.
            let read = match conn.recv(&mut conn_paths, pkt_buf, recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("{} recv failed: {:?}", conn.trace_id(), e);
                    continue 'read;
                },
            };

            trace!("{} processed {} bytes", conn.trace_id(), read);

            // Create a new application protocol session as soon as the QUIC
            // connection is established.
            if !*app_proto_selected &&
                (conn.is_in_early_data() ||
                    conn.is_established())
            {
                // At this stage the ALPN negotiation succeeded and selected a
                // single application protocol name. We'll use this to construct
                // the correct type of HttpConn but `application_proto()`
                // returns a slice, so we have to convert it to a str in order
                // to compare to our lists of protocols. We `unwrap()` because
                // we need the value and if something fails at this stage, there
                // is not much anyone can do to recover.
                let app_proto = conn.application_proto();

                #[allow(clippy::box_default)]
                if alpns::HTTP_3.contains(&app_proto) {
                    let dgram_sender = if conn_args.dgrams_enabled {
                        Some(Http3DgramSender::new(
                            conn_args.dgram_count,
                            conn_args.dgram_data.clone(),
                            1,
                        ))
                    } else {
                        None
                    };

                    *http_conn = match MulticoreHttp3Conn::with_conn(
                        &mut conn,
                        conn_args.max_field_section_size,
                        conn_args.qpack_max_table_capacity,
                        conn_args.qpack_blocked_streams,
                        dgram_sender,
                    ) {
                        Ok(v) => Some(v),

                        Err(e) => {
                            trace!("{} {}", conn.trace_id(), e);
                            None
                        },
                    };

                    *app_proto_selected = true;
                } else {
                    error!("HTTP version not Http3");
                    break 'main;
                }

                // Update max_datagram_size after connection established.
                *max_datagram_size = conn.max_send_udp_payload_size(conn_paths.get_max_datagram_size());
            }

            if http_conn.is_some() {
                let http_conn = http_conn.as_mut().unwrap();
                let mut partial_responses = client_guard.partial_responses.lock();
                let mut partial_requests = client_guard.partial_requests.lock();

                // Handle writable streams.
                for stream_id in conn.writable() {
                    http_conn.handle_writable(&mut conn, &mut partial_responses, stream_id);
                }

                if http_conn
                    .handle_requests(
                        &mut conn,
                        &mut conn_paths,
                        &mut partial_requests,
                        &mut partial_responses,
                        &args.root,
                        &args.index,
                        &mut buf,
                    )
                    .is_err()
                {
                    continue 'read;
                }
            }

            handle_path_events(client, schedulers.get_mut(&client.client_id).unwrap());

            // See whether source Connection IDs have been retired.
            while let Some(retired_scid) = client.conn.retired_scid_next() {
                info!("Retiring source CID {:?}", retired_scid);
                clients_ids.remove(&retired_scid);
            }

            // Provides as many CIDs as possible.
            while client.conn.source_cids_left() > 0 {
                let (scid, reset_token) = generate_cid_and_reset_token(&rng);
                if client
                    .conn
                    .new_source_cid(&scid, reset_token, false)
                    .is_err()
                {
                    break;
                }

                clients_ids.insert(scid, client.client_id);
            }
        } */
    }
}


fn handle_path_events(client: &mut Client, scheduler: &mut Scheduler) {
    let paths_sockets_addrs: Vec<(std::net::SocketAddr, std::net::SocketAddr)> = client.conn_paths.iter().map(|(_, p)| (p.local_addr(), p.peer_addr())).collect();
    for (local_addr, peer_addr) in paths_sockets_addrs{
        loop {
            match client.conn_paths.pop_event(local_addr, peer_addr) {
                Some(qe) => {
                    match qe {
                        quiche::PathEvent::New(local_addr, peer_addr) => {
                            info!(
                                "{} Seen new path ({}, {})",
                                client.conn.trace_id(),
                                local_addr,
                                peer_addr
                            );
            
                            scheduler.add_path((local_addr, peer_addr));
            
                            // Directly probe the new path.
                            client
                                .conn
                                .probe_path(&mut client.conn_paths,local_addr, peer_addr)
                                .map_err(|e| error!("cannot probe: {}", e))
                                .ok();
                        },
            
                        quiche::PathEvent::Validated(local_addr, peer_addr) => {
                            info!(
                                "{} Path ({}, {}) is now validated",
                                client.conn.trace_id(),
                                local_addr,
                                peer_addr
                            );
                            if client.conn_paths.multipath() {
                                match client.conn_paths
                                    .set_active( local_addr, peer_addr, true)
                                    .map_err(|e| error!("cannot set path active: {}", e))
                                    .ok()
                                {
                                    Some(()) => scheduler.add_path((local_addr, peer_addr)),
                                    None => (),
                                }
                            }
                        },
            
                        quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                            info!(
                                "{} Path ({}, {}) failed validation",
                                client.conn.trace_id(),
                                local_addr,
                                peer_addr
                            );
                        },
            
                        quiche::PathEvent::Closed(local_addr, peer_addr, err, reason) => {
                            info!(
                                "{} Path ({}, {}) is now closed and unusable; err = {} reason = {:?}",
                                client.conn.trace_id(),
                                local_addr,
                                peer_addr,
                                err,
                                reason,
                            );
                            scheduler.remove_path((local_addr, peer_addr))
                        },
            
                        quiche::PathEvent::ReusedSourceConnectionId(cid_seq, old, new) => {
                            info!(
                                "{} Peer reused cid seq {} (initially {:?}) on {:?}",
                                client.conn.trace_id(),
                                cid_seq,
                                old,
                                new
                            );
                        },
            
                        quiche::PathEvent::PeerMigrated(local_addr, peer_addr) => {
                            info!(
                                "{} Connection migrated to ({}, {})",
                                client.conn.trace_id(),
                                local_addr,
                                peer_addr
                            );
                            scheduler.add_path((local_addr, peer_addr)) // TODO check if valid
                        },
            
                        quiche::PathEvent::PeerPathStatus(addr, path_status) => {
                            info!("Peer asks status {:?} for {:?}", path_status, addr,);
                            client
                                .conn_paths
                                .set_path_status( addr.0, addr.1, path_status, false)
                                .map_err(|e| error!("cannot follow status request: {}", e))
                                .ok();
                        },

                        quiche::PathEvent::PacketNumSpaceDiscarded((local, peer), epoch, handshake_status, now) => {
                            let p = client.conn_paths.get_mut_from_addr(local, peer).unwrap();
                            p.recovery.on_pkt_num_space_discarded(epoch, handshake_status, now);
                        },
                    }
                }, 
                None => break,
            }
        }
    }
}

/// Set SO_REUSEPORT socket option.
///
/// Note that this socket option is set only on linux platforms.
#[cfg(target_os = "linux")]
fn set_so_reuseport_sockopt(sock: &mio::net::UdpSocket) -> io::Result<()> {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::ReusePort;
    use std::os::unix::io::AsRawFd;

    // mio::net::UdpSocket doesn't implement AsFd (yet?).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(sock.as_raw_fd()) };

    setsockopt(&fd, ReusePort, &true)?;
    
    Ok(())
}

pub fn multicore_start_server(args: ServerArgs, conn_args: CommonArgs) {
    env_logger::builder()
        .default_format_timestamp_nanos(true)
        .init();

    let mut pacing = false;

    // Dumy socket used for detecting gso 
    let socket = mio::net::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(); 

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

    config.set_application_protos(&conn_args.alpns).unwrap();

    config.set_max_idle_timeout(conn_args.idle_timeout);
    config.set_max_recv_udp_payload_size(max_datagram_size);
    config.set_max_send_udp_payload_size(max_datagram_size);
    config.set_initial_max_data(conn_args.max_data);
    config.set_initial_max_stream_data_bidi_local(conn_args.max_stream_data);
    config.set_initial_max_stream_data_bidi_remote(conn_args.max_stream_data);
    config.set_initial_max_stream_data_uni(conn_args.max_stream_data);
    config.set_initial_max_streams_bidi(conn_args.max_streams_bidi);
    config.set_initial_max_streams_uni(conn_args.max_streams_uni);
    config.set_disable_active_migration(!conn_args.enable_active_migration);
    config.set_active_connection_id_limit(conn_args.max_active_cids);
    config.set_initial_congestion_window_packets(
        usize::try_from(conn_args.initial_cwnd_packets).unwrap(),
    );
    config.set_multipath(conn_args.multipath);

    config.set_max_connection_window(conn_args.max_window);
    config.set_max_stream_window(conn_args.max_stream_window);

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

    if conn_args.early_data {
        config.enable_early_data();
    }

    if conn_args.no_grease {
        config.grease(false);
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

    let local_addr = args.listen.parse().unwrap();

    let mut threads_poll = Poll::new().unwrap();
    let mut threads_done_event = Events::with_capacity(1024);
    let threads_done_token = Token(0);
    let waker = Arc::new(Waker::new(threads_poll.registry(), threads_done_token).unwrap());

    drop(socket); // Garbage collect the initial socket

    loop {
        let mut client = wait_for_new_connection(&mut config, &args, &conn_args, waker.clone(), &mut keylog, local_addr);
        info!("Going to wait for threads connection is established");
        let peer_addr = client.conn_paths.lock().get_active_mut().unwrap().peer_addr().into();
        let guarded_client = Arc::new(client);
        let clonn_guarded = guarded_client.clone();
        thread::spawn(move || {
            server_thread(clonn_guarded, (local_addr, peer_addr))
        });
        threads_poll.poll(&mut threads_done_event, None).unwrap();
    }
}


fn wait_for_new_connection(
    config: &mut Config, args: &ServerArgs, conn_args: &CommonArgs, waker: Arc<Waker>,
    keylog: &mut Option<File>, local_addr: std::net::SocketAddr
) -> MulticoreClient {
    let rng = SystemRandom::new();

    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);
    let mut socket = mio::net::UdpSocket::bind(local_addr).unwrap();

    let conn_id_seed = ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &rng).unwrap();
    poll.registry().register(&mut socket,  Token(0), Interest::READABLE).unwrap();    
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

            // TODO Add packet dump

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

            #[allow(unused_mut)]
            let (mut conn, mut conn_paths) = quiche::accept(
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

            // Only bother with qlog if the user specified it.
            #[cfg(feature = "qlog")]
            {
                if let Some(dir) = std::env::var_os("QLOGDIR") {
                    let id = format!("{:?}", &scid);
                    let writer = make_qlog_writer(&dir, "server", &id);

                    conn.set_qlog(
                        std::boxed::Box::new(writer),
                        "quiche-server qlog".to_string(),
                        format!("{} id={}", "quiche-server qlog", id),
                    );
                }
            }

            return MulticoreClient {
                conn: Arc::new(Mutex::new(conn)),
                conn_paths: Arc::new(Mutex::new(conn_paths)),
                http_conn: Arc::new(Mutex::new(None)),
                partial_requests: Arc::new(Mutex::new( HashMap::new())),
                partial_responses: Arc::new(Mutex::new( HashMap::new())),
                app_proto_selected: Arc::new(Mutex::new( false)),
                max_datagram_size: Arc::new(Mutex::new( MAX_DATAGRAM_SIZE)),
                loss_rate: Arc::new(Mutex::new( 0.0)),
                max_send_burst: Arc::new(Mutex::new( MAX_BUF_SIZE)),
                paths_thread: Arc::new(Mutex::new( Vec::new())),
                // args: Arc::new(*args.clone()),
                // conn_args: Arc::new(*conn_args.clone()), 
                on_connection_done: waker.clone()
            };
        }
    }
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

#[cfg(not(target_os = "linux"))]
fn set_txtime_sockopt(_: &mio::net::UdpSocket) -> io::Result<()> {
    use std::io::Error;
    use std::io::ErrorKind;

    Err(Error::new(
        ErrorKind::Other,
        "Not supported on this platform",
    ))
}

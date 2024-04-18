
use std::io;
use std::convert::TryFrom;
use std::fs::File;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
use quiche::{MulticorePath, MulticoreConnection, Config};
use ring::rand::*;
use std::sync::Arc;

use mio::{Events, Poll, Token, Waker, Interest};
use crate::args::*;
use crate::common::*;
use crate::sendto::*;

const MAX_BUF_SIZE: usize = 65507;
const MAX_DATAGRAM_SIZE: usize = 1350;

fn server_thread(
    client: Arc<MulticoreClient>,
    mut path: MulticorePath, 
    args: ServerArgs
) -> () {
    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];

    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    let local_addr = path.local_addr();
    let peer_addr: std::net::SocketAddr = path.peer_addr();
    let (socket, pacing, enable_gso) = 
        multicore_create_socket(&local_addr,&peer_addr, &mut poll, args.disable_pacing, args.disable_gso);
    
    // All the followings are one time flags. They switch from false to true
    let mut continue_write = false;
    let mut loss_rate ;
    let max_datagram_size = MAX_DATAGRAM_SIZE; 
    let mut max_send_burst = MAX_BUF_SIZE;

    let mut create_send_stream = false;
    let mut is_established = false;
    let mut is_in_early_data = false;

    // let to_send = (2 as usize).pow(29); // 500MB to send
    let mut stream_id = None;
    loop {
        let timeout = match continue_write {
            true => Some(Duration::ZERO), 
            false => Some(Duration::from_millis(10))
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

            // Process potentially coalesced packets.
            let read = match path.recv(&client.conn, &mut buf[..len], recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("[Path Thread] recv failed: {:?}", e);
                    continue 'read;
                },
            };

            trace!("[Path Thread] processed {} bytes", read);
            
            if !create_send_stream  {
                let conn = client.conn.read().unwrap();
                if !(is_in_early_data || is_established){
                    let app_proto = conn.application_proto();
                    
                    #[allow(clippy::box_default)]
                    if alpns::MMPQUIC.contains(&app_proto) {
                        create_send_stream = true;
                        {
                            let mut next_stream_id = client.next_stream_id.write().unwrap();
                            stream_id = Some(next_stream_id.clone());
                            *next_stream_id += 1;
                        }
                    } else if alpns::HTTP_3.contains(&app_proto) {
                        let mut conn = client.conn.write().unwrap();
                        error!("APLN not mMPQUIC");
                        conn.close(true, 0, b"APLN not mMPQUIC".as_slice()).unwrap();
                    }

                } else {
                    is_in_early_data = conn.is_in_early_data();
                    is_established = conn.is_established();
                }
            }

            if stream_id.is_some() && create_send_stream {

            }
        }



        handle_path_events(&mut path, &client.conn);

        continue_write = false;
        loss_rate = path.stats().lost as f64 / path.stats().sent as f64;
        if loss_rate > loss_rate + 0.001 {
            max_send_burst = max_send_burst / 4 * 3;
            // Minimun bound of 10xMSS.
            max_send_burst = max_send_burst.max(max_datagram_size * 10);
        }

        let max_send_burst =
            path.send_quantum().min(max_send_burst) / max_datagram_size * max_datagram_size;
        let mut total_write = 0;
        let mut dst_info: Option<quiche::SendInfo> = None;

        while total_write < max_send_burst {
            let res = path.send_on_path(
                &client.conn, 
                &mut out[total_write..max_send_burst],
            );

            let (write, send_info) = match res {
                Ok(v) => v,

                Err(quiche::Error::Done) => {
                    trace!("{} <-> {} done writing", path.local_addr(), path.peer_addr());
                    break;
                },

                Err(e) => {
                    error!("{} <-> {} send failed: {:?}",  path.local_addr(), path.peer_addr(), e);
                    let mut conn = client.conn.write().unwrap();
                    conn.close(false, 0x1, b"fail").ok();
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
                }
                panic!("send_to() failed: {:?}", e);
            }

            trace!("written {} bytes", total_write);

            if continue_write {
                trace!("pause writing");
            } else if total_write >= max_send_burst {
                trace!("pause writing",);
                continue_write = true;
            }
        }
    }
}

pub fn multicore_start_server(args: ServerArgs, conn_args: CommonArgs){
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
        let (conn, init_path) = wait_for_new_connection(&mut config, &args, &mut keylog, local_addr);
        info!("Received new connection");
        let client = MulticoreClient{
            conn: Arc::new(RwLock::new(conn)), 
            on_done: waker.clone(), 
            next_stream_id: Arc::new(RwLock::new(3))
        };
        let client = Arc::new(client);
        let copied_args = args.clone();
        thread::spawn(move || {
            server_thread(
                client, 
                init_path, 
                copied_args
            )
        });
        threads_poll.poll(&mut threads_done_event, None).unwrap();

    }

}

fn wait_for_new_connection(
    config: &mut Config, args: &ServerArgs,
    keylog: &mut Option<File>, local_addr: std::net::SocketAddr
) -> (MulticoreConnection, MulticorePath) {
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

            #[allow(unused_mut)]
            let (mut conn, init_path) = quiche::multicore_accept(
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

            info!("early ? {} established {}", conn.is_in_early_data(), conn.is_established());
            // TODO multicore qlog
            return (conn, init_path)
        }
    }
}

fn multicore_create_socket(
    local_addr: &std::net::SocketAddr, 
    peer_addr: &std::net::SocketAddr,
    poll: &mut mio::Poll, 
    disable_pacing: bool, 
    disable_gso: bool
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

    info!("[Server Thread] Thread ID : {:?} Started with path {:?} <-> {:?}", 
        thread::current().id(), 
        local_addr, 
        peer_addr
    );

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

fn handle_path_events(path: &mut MulticorePath, conn_guard: &Arc<RwLock<MulticoreConnection>>) {
    while let Some(qe) = path.path_event_next() {
        match qe {
            quiche::PathEvent::New(local_addr, peer_addr) => {
                info!(
                    "Seen new path ({}, {})",
                    local_addr,
                    peer_addr
                );

                // Directly probe the new path.
                path
                    .probe_path(conn_guard)
                    .map_err(|e| error!("cannot probe: {}", e))
                    .ok();
            },

            quiche::PathEvent::Validated(local_addr, peer_addr) => {
                info!(
                    "Path ({}, {}) is now validated",
                    local_addr,
                    peer_addr
                );
                if path.multipath {
                    path
                        .set_active(true)
                        .map_err(|e| error!("cannot set path active: {}", e))
                        .ok();
                }
            },

            quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                info!(
                    "Path ({}, {}) failed validation",
                    local_addr,
                    peer_addr
                );
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
                    cid_seq,
                    old,
                    new
                );
            },

            quiche::PathEvent::PeerMigrated(local_addr, peer_addr) => {
                info!(
                    "Connection migrated to ({}, {})",
                    local_addr,
                    peer_addr
                );
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

use governor::Quota;
use mineshare::*;
use rand::seq::IndexedRandom;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::select;
use tokio::time::Instant;
use tokio_util::compat::FuturesAsyncReadCompatExt as _;
use tracing::Instrument as _;
use tracing::{info, info_span, trace};
use yamux::Stream;

use clap::Parser;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio_util::compat::TokioAsyncReadCompatExt;

use yamux::{Config, Connection};

#[tokio::main]
async fn main() {
    async_main().await
}

async fn async_main() {
    let args = Args::parse();
    let base_domain: Arc<str> = Arc::from(args.base_domain);
    let map = Arc::new(RwLock::new(HashMap::<
        Vec<u8>,
        mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>,
    >::new()));
    let server_listener = match TcpListener::bind(&args.server_socket_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "Failed to start listening on server addr `{}`: {e}",
                args.server_socket_addr
            );
            std::process::exit(1);
        }
    };
    let client_listener = match TcpListener::bind(&args.client_socket_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "Failed to start listening on client addr`{}`: {e}",
                args.client_socket_addr
            );
            std::process::exit(1);
        }
    };
    let global_counter = Arc::new(AtomicU64::new(0));
    let base_domain2 = base_domain.clone();
    let map2 = map.clone();
    tokio::task::spawn(async move {
        loop {
            let accepted = client_listener.accept().await.expect("Failed to listen");
            let map = map2.clone();
            let base_domain = base_domain2.clone();
            tokio::task::spawn(async move {
                let (mut stream, addr) = accepted;
                // TODO: check ratelimit for IP
                let mut data = vec![0u8; 2048];
                let mut cursor = 0;
                let span = info_span!("parse_host_address");
                let fut = async {
                    loop {
                        match stream.read(&mut data[cursor..]).await {
                            Ok(v) => {
                                cursor += v;
                            }
                            Err(_e) => {
                                info!("client {addr} exited without valid initial packet (cannot parse)");
                                return Err(());
                            }
                        };
                        let access = match try_parse_init_packet(&data[..cursor], addr, base_domain.as_bytes()) {
                            Ok(Some(v)) => v,
                            Ok(None) => {
                                continue;
                            }
                            Err(_e) => {
                                return Err(())
                            },
                        };
                        let map = map.read().await;
                        if let Some(sender) = map.get(access) {
                            _ = sender.send((stream, data, addr)).await;
                            return Ok(());
                        }
                    }
                }
                .instrument(span);
                let timeout = tokio::time::timeout(Duration::from_secs(15), fut);
                match timeout.await {
                    Ok(Ok(())) => {
                        trace!("client {addr} sent to handler thread")
                    }
                    Ok(Err(())) => {
                        // Error should have been logged already in the async block
                    }
                    Err(_) => {
                        info!("client {addr} timed out during initial connection");
                    }
                }
            });
        }
    });
    let map2 = map.clone();
    let global_counter2 = global_counter.clone();
    tokio::task::spawn(async move {
        let map = map2;
        loop {
            let global_counter = global_counter2.clone();
            let (stream, addr) = server_listener
                .accept()
                .await
                .expect("Accepting client shouldn't fail");
            let map = map.clone();
            tokio::task::spawn(server_handler(stream, addr, map, global_counter));
        }
    });

    tokio::signal::ctrl_c().await.unwrap();
}

#[allow(clippy::type_complexity)]
async fn server_handler(
    mut server_stream: TcpStream,
    addr: SocketAddr,
    map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>>>,
    counter: Arc<AtomicU64>,
) {
    let (mut new_client_recv, prefix) = {
        let mut locked = map.write().await;
        let mut prefix = get_random_prefix(&locked);
        prefix.push(b'.');
        let url = prefix.clone();
        if Message::DomainDecided(&url)
            .encode(&mut server_stream)
            .await
            .is_err()
        {
            return;
        }
        let (send, recv) = mpsc::channel::<(TcpStream, Vec<u8>, SocketAddr)>(10);
        locked.insert(url, send);
        (recv, prefix)
    };
    let closure = async move {
        let mut yamux_config = Config::default();
        yamux_config
            .set_max_connection_receive_window(Some(TOTAL_BUF_SIZE))
            .set_max_num_streams(MAX_CONN_COUNT + 1) // Account for heartbeat
            .set_read_after_close(true);

        let mut multiplexed =
            Connection::new(server_stream.compat(), yamux_config, yamux::Mode::Client);

        let fut = std::future::poll_fn(|ctx| multiplexed.poll_new_outbound(ctx));

        let mut heartbeat_stream = match fut.await {
            Ok(s) => s.compat(),
            Err(e) => {
                info!("Failed to create heartbeat connection with {addr}: {e}");
                return;
            }
        };
        let mut count = 0;
        let mut timeout_at = Instant::now() + Duration::from_secs(90);
        let mut heartbeat_at = Instant::now() + Duration::from_secs(15);
        let mut heartbeat_buf = [0u8; 256];
        loop {
            select! {
                new = new_client_recv.recv() => {
                    if count >= MAX_CONN_COUNT {
                        // TODO: Disconnect politely
                        return;
                    }
                    let (stream, data, clientaddr) = new.expect("Sender wasn't dropped from map");
                    let limiter = Limiter::direct(Quota::per_second(LIMIT_MEGABYTE_PER_SECOND).allow_burst(LIMIT_BURST));
                    let server_stream = std::future::poll_fn(|ctx| multiplexed.poll_new_outbound(ctx)).await;
                    let server_stream = match server_stream {
                        Ok(s) => s,
                        Err(e) => {
                            info!("Creating connection to server {addr} failed: {e}");
                            continue;
                        },
                    };
                    count += 1;
                    tokio::task::spawn(handle_duplex(stream, data, addr, clientaddr, server_stream, limiter, counter.clone()));
                }
                _heartbeat = tokio::time::sleep_until(heartbeat_at) => {
                    match heartbeat_stream.write_all(b"heartbeat").await {
                        Ok(_r) => {},
                        Err(e) => {
                            info!("Sending heartbeat to server {addr} failed: {e}");
                            _ = std::future::poll_fn(|ctx| multiplexed.poll_close(ctx)).await;
                            return;
                        },
                    };
                    heartbeat_at = Instant::now() + Duration::from_secs(15);
                }
                _timeout = tokio::time::sleep_until(timeout_at) => {
                    info!("Server {addr} failed to respond to heartbeat");
                    return;
                }
                amt = heartbeat_stream.read(&mut heartbeat_buf) => {
                    match amt {
                        Ok(0) => {
                            _ = std::future::poll_fn(|ctx| multiplexed.poll_close(ctx)).await;
                            return;
                        }
                        Ok(amt) => {
                            if amt > 10 {
                                // Invalid amt of data sent
                                info!("Invalid amount of data received for heartbeat from {addr}");
                                _ = std::future::poll_fn(|ctx| multiplexed.poll_close(ctx)).await;
                                return;
                            }
                            timeout_at = Instant::now() + Duration::from_secs(90);
                        },
                        Err(_e) => {
                            info!("Failed to read heartbeat message");
                            _ = std::future::poll_fn(|ctx| multiplexed.poll_close(ctx)).await;
                            return;
                        },
                    }
                }
            }
        }
    };
    closure.await;
    let mut locked = map.write().await;
    locked.remove(&prefix);
}

async fn handle_duplex(
    mut client_stream: TcpStream,
    data: Vec<u8>,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    server_stream: Stream,
    limiter: Limiter,
    counter: Arc<AtomicU64>,
) {
    let mut server_stream = server_stream.compat();
    match server_stream.write_all(&data).await {
        Ok(()) => (),
        Err(e) => {
            info!("Failed to write {client_addr}'s initial packet to server: {e}");
            // TODO: politely disconnect client
            return;
        }
    }

    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];

    loop {
        select! {
            res = client_stream.read(&mut buf1) => {
                match res {
                    Ok(0) => {
                        return;
                    }
                    Ok(amt) => {
                        limiter
                            .until_n_ready(NonZeroU32::new(amt as u32).unwrap())
                            .await
                            .expect("Buffer size < Burst quota");
                        counter.fetch_add(amt as u64, Ordering::Relaxed);
                        if let Err(e) = server_stream.write_all(&buf1[..amt]).await {
                            info!("Error when writing to server {server_addr} by client {client_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = client_stream.shutdown().await;
                            _ = server_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr}: {e}");
                        // If one of the connections errors, we should abort the other one too.
                        // We ignore the returned results because there's nothing we can do if the disconnection fails.
                        _ = client_stream.shutdown().await;
                        _ = server_stream.shutdown().await;
                        return;
                    },
                }
            }
            res = server_stream.read(&mut buf2) => {
                match res {
                    Ok(0) => {
                        return;
                    }
                    Ok(amt) => {
                        limiter
                            .until_n_ready(NonZeroU32::new(amt as u32).unwrap())
                            .await
                            .expect("Buffer size < Burst quota");
                        counter.fetch_add(amt as u64, Ordering::Relaxed);
                        if let Err(e) = client_stream.write_all(&buf1[..amt]).await {
                            info!("Error when writing to client {client_addr} by server {server_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = server_stream.shutdown().await;
                            _ = client_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr} by server {server_addr}: {e}");
                        // If one of the connections errors, we should abort the other one too.
                        // We ignore the returned results because there's nothing we can do if the disconnection fails.
                        _ = server_stream.shutdown().await;
                        _ = client_stream.shutdown().await;
                        return;
                    },
                }
            }
        }
    }
}

// #[allow(clippy::type_complexity)]
// async fn server_handler(
//     mut stream: TcpStream,
//     addr: SocketAddr,
//     map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>)>>>>,
//     base_domain: Arc<str>,
//     counter: Arc<AtomicU64>,
// ) {
//     let mut recv = {
//         let mut locked = map.write().await;
//         let mut prefix = get_random_prefix(&locked);
//         prefix.push(b'.');
//         prefix.extend(base_domain.as_bytes());
//         let url = prefix;
//         if Message::DomainDecided(&url)
//             .encode(&mut stream)
//             .await
//             .is_err()
//         {
//             return;
//         }
//         let (send, recv) = mpsc::channel::<(TcpStream, Vec<u8>)>(10);
//         locked.insert(url, send);
//         recv
//     };

//     let (server_recv, server_send) = stream.into_split();
//     let server_send = Arc::new(Mutex::new(server_send));
//     let (client_send, client_recv) = mpsc::channel::<Client>(10);
//     let (disconnect_toserver_send, disconnect_toserver_recv) = mpsc::channel::<u64>(10);
//     let (disconnect_toclient_send, mut disconnect_toclient_recv) = mpsc::channel::<u64>(10);

//     let server_err_notify = Arc::new(Notify::new());

//     let server_joinhandle = tokio::task::spawn(handle_server_to_client(ServerToClientArgs {
//         server_stream: server_recv,
//         server_send: server_send.clone(),
//         clients: client_recv,
//         client_disconnects_send: disconnect_toclient_send,
//         client_disconnects_recv: disconnect_toserver_recv,
//         counter: counter.clone(),
//     }));

//     let mut id = 0;
//     let mut client_joins: HashMap<u64, JoinHandle<()>> = HashMap::new();
//     loop {
//         select! {
//             id = disconnect_toclient_recv.recv() => {
//                 match id {
//                     Some(id) => {
//                         if let Some(task) = client_joins.remove(&id) {
//                             info!("Client #{id} connected from {addr}. Aborting client");
//                             task.abort();
//                         } else {
//                             warn!("Server notified that client {id} disconnected. However, this ID does not exist!");
//                         };
//                     },
//                     None => {
//                         info!("Server thread returned");
//                         // This means server thread either ran into an error or disconnected properly. Either way, we disconnect all other connections.
//                         client_joins.values().for_each(|h| h.abort());
//                         server_joinhandle.abort();
//                         return;
//                     },
//                 }
//             }
//             _err = server_err_notify.notified() => {
//                 // Server connection was lost
//                 warn!("A client notified that server {addr} errored when sending. Aborting all connections to server.");
//                 client_joins.values().for_each(|h| h.abort());
//                 server_joinhandle.abort();
//                 return;
//             }
//             c = recv.recv() => {
//                 id += 1;
//                 let (client, data) = c.expect("Sender should still be in the hashmap");
//                 {
//                     let mut server_send = server_send.lock().await;
//                     if Message::Connect(id).encode(&mut *server_send).await.is_err() {
//                         warn!("Server {addr} failed to receive message. Aborting.");
//                         client_joins.values().for_each(|h| h.abort());
//                         server_joinhandle.abort();
//                         return;
//                     }
//                     if Message::Data(id, &data).encode(&mut *server_send).await.is_err() {
//                         warn!("Server {addr} failed to receive message. Aborting.");
//                         client_joins.values().for_each(|h| h.abort());
//                         server_joinhandle.abort();
//                         return;
//                     }
//                 }
//                 let limiter = Arc::new(Limiter::direct(
//                     Quota::per_second(NonZeroU32::new(LIMIT_MEGABYTE_PER_SECOND).unwrap())
//                         .allow_burst(NonZeroU32::new(LIMIT_BURST).unwrap()),
//                 ));
//                 let (ack_send, ack_recv) = mpsc::channel::<usize>(100);
//                 let (client_read, client_write) = client.into_split();
//                 client_joins.insert(id, tokio::task::spawn(handle_client_to_server(ClientToServerArgs {
//                     server_stream: server_send.clone(),
//                     client_stream: client_read,
//                     limiter: limiter.clone(),
//                     server_stream_error: server_err_notify.clone(),
//                     client_disconnects_send: disconnect_toserver_send.clone(),
//                     ack: ack_recv,
//                     id,
//                     counter: counter.clone(),
//                 })));
//                 if client_send.send(Client {
//                     id,
//                     writer: client_write,
//                     limiter,
//                     buf: Box::new([0u8; RECV_BUF_SIZE]),
//                     buf_len: 0,
//                     ack: ack_send,
//                 }).await.is_err() {
//                     // This means server thread either ran into an error or disconnected properly. Either way, we disconnect all other connections.
//                     client_joins.values().for_each(|h| h.abort());
//                     server_joinhandle.abort();
//                     return;
//                 };
//             }
//         }
//     }
// }

// struct ClientToServerArgs {
//     server_stream: Arc<Mutex<OwnedWriteHalf>>,
//     client_stream: OwnedReadHalf,
//     limiter: Arc<Limiter>,
//     server_stream_error: Arc<Notify>,
//     client_disconnects_send: mpsc::Sender<u64>,
//     ack: mpsc::Receiver<usize>,
//     counter: Arc<AtomicU64>,
//     id: u64,
// }

// struct Client {
//     id: u64,
//     writer: OwnedWriteHalf,
//     limiter: Arc<Limiter>,
//     buf: Box<[u8]>,
//     buf_len: usize,
//     ack: mpsc::Sender<usize>,
// }

// struct ServerToClientArgs {
//     server_stream: OwnedReadHalf,
//     server_send: Arc<Mutex<OwnedWriteHalf>>,
//     clients: mpsc::Receiver<Client>,
//     client_disconnects_send: mpsc::Sender<u64>,
//     client_disconnects_recv: mpsc::Receiver<u64>,
//     counter: Arc<AtomicU64>,
// }

// async fn handle_server_to_client(args: ServerToClientArgs) {
//     let ServerToClientArgs {
//         server_stream: mut server,
//         server_send,
//         clients: mut recv,
//         client_disconnects_send,
//         mut client_disconnects_recv,
//         counter,
//     } = args;

//     fn remove_client(
//         id: u64,
//         clients: &mut Vec<Client>,
//         map: &mut HashMap<u64, usize>,
//     ) -> Option<()> {
//         let index = map.remove(&id)?;
//         clients.swap_remove(index);
//         let new_client = match clients.get(index) {
//             Some(c) => c,
//             None => {
//                 // There's 0 clients in the queue, so no need for swapping
//                 return Some(());
//             }
//         };
//         // Update the hashmap entry
//         *map.get_mut(&new_client.id).unwrap() = index;
//         Some(())
//     }

//     let mut clients: Vec<Client> = Vec::new();
//     let mut id_to_index: HashMap<u64, usize> = HashMap::new();

//     let mut buf = [0u8; MUX_RECV_BUF_SIZE];
//     let mut buf_start: usize = 0;

//     let mut timeout_when = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
//     let mut timeout_count = 0;

//     loop {
//         let time = timeout_when;
//         let timeout = tokio::time::sleep_until(time);
//         // TODO handle clients SEND as well
//         select! {
//             id = client_disconnects_recv.recv() => {
//                 match id {
//                     Some(n) => {
//                         debug_assert!(remove_client(n, &mut clients, &mut id_to_index).is_some());
//                     },
//                     // If there's no other end, that means the spawner thread is dead
//                     // which shouldn't happen
//                     None => unreachable!(),
//                 }
//             }
//             _ = timeout => {
//                 if timeout_count > 4 {
//                     // Server is probably dead
//                     return;
//                 }
//                 timeout_count += 1;
//                 let mut server = server_send.lock().await;
//                 if Message::HeartBeat.encode(&mut *server).await.is_err() {
//                     return;
//                 }
//             }
//             data_amt = server.read(&mut buf[buf_start..]) => {
//                 match data_amt {
//                     Ok(len) => {
//                         buf_start += len;
//                         loop {
//                             let (msg, len) = match Message::decode(&buf[0..buf_start]) {
//                                 Ok(None) => {
//                                     break;
//                                 },
//                                 Ok(Some((msg, len))) => {
//                                     (msg, len)
//                                 }
//                                 Err(_e) => {
//                                     // If it errored, that means the server made an error. We can't be sure if
//                                     // any further communiations would be valid or not, so we just exit
//                                     return;
//                                 }
//                             };

//                             match msg {
//                                 Message::DomainDecided(_) | Message::Connect(_) => {
//                                     // We should never get these packets from the server
//                                     return;
//                                 },
//                                 Message::Disconnect(id) => {
//                                     if let Some(()) = remove_client(id, &mut clients, &mut id_to_index) {
//                                         // We confirmed that client is removed from the vec & map
//                                         // so we notify the spawner thread to remove the task with
//                                         // this ID
//                                         client_disconnects_send.send(id).await.unwrap();
//                                     }
//                                 },
//                                 Message::Data(id, items) => 'blck: {
//                                     let Some(&index) = id_to_index.get(&id) else {
//                                         // Server sent invalid ID
//                                         break 'blck
//                                     };
//                                     let client = &mut clients[index];
//                                     let mut buf = &mut client.buf[client.buf_len..];
//                                     // If buffer isn't empty and the limiter has enough quota
//                                     // then we can just try to send here and now.
//                                     if !buf.is_empty() && client.limiter.check_n(NonZeroU32::new(buf.len() as u32).unwrap())
//                                         .unwrap().is_ok()
//                                     {
//                                         if let Ok(wrote) = client.writer.try_write(buf) {
//                                             counter.fetch_add(wrote as u64, Ordering::Relaxed);
//                                             buf.copy_within(wrote.., 0);
//                                             client.buf_len -= wrote;
//                                             buf = &mut client.buf[client.buf_len..];
//                                             let mut server = server_send.lock().await;
//                                             if Message::Acknowledged(id, wrote).encode(&mut *server).await.is_err() {
//                                                 // Server is dead
//                                                 return;
//                                             }
//                                         }
//                                     }
//                                     if let Some(buf) = buf.get_mut(..items.len()) {
//                                         buf.copy_from_slice(items);
//                                         client.buf_len += items.len();

//                                     } else {
//                                         // Server sent too much data
//                                         return;
//                                     }
//                                     // See if we can continue writing the new data too
//                                     if !buf.is_empty() && client.limiter.check_n(NonZeroU32::new(buf.len() as u32).unwrap())
//                                         .unwrap().is_ok()
//                                     {
//                                         if let Ok(wrote) = client.writer.try_write(buf) {
//                                             counter.fetch_add(wrote as u64, Ordering::Relaxed);
//                                             buf.copy_within(wrote.., 0);
//                                             client.buf_len -= wrote;
//                                             let mut server = server_send.lock().await;
//                                             if Message::Acknowledged(id, wrote).encode(&mut *server).await.is_err() {
//                                                 // Server is dead
//                                                 return;
//                                             }
//                                         }
//                                     }
//                                 },
//                                 Message::HeartBeat => (),
//                                 Message::Acknowledged(id, acked) => 'blck: {
//                                     let Some(&index) = id_to_index.get(&id) else {
//                                         // Server sent invalid ID
//                                         break 'blck
//                                     };
//                                     // Send ACK to the corresponding client sender so
//                                     // it can continue to send data
//                                     clients[index].ack.send(acked).await.ok();
//                                 },
//                             }

//                             buf.copy_within(len.., 0);
//                             buf_start -= len;
//                         }
//                         timeout_when = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
//                     },
//                     Err(_e) => {
//                         // Server connection failed. Once we return everything will be shut down
//                         // by spawner thread.
//                         return;
//                     },
//                 }
//             }
//             client = recv.recv() => {
//                 match client {
//                     Some(c) => {
//                         let index = clients.len();
//                         let id = c.id;
//                         clients.push(c);
//                         debug_assert!(id_to_index.insert(id, index).is_none());
//                     },
//                     None => {
//                         // If this is reached, it means the spawner thread died without shutting us down
//                         // which should never happen.
//                         unreachable!()
//                     }
//                 }
//             }
//         }
//     }
// }
// async fn handle_client_to_server(args: ClientToServerArgs) {
//     let ClientToServerArgs {
//         server_stream: server,
//         client_stream: mut client,
//         limiter,
//         server_stream_error,
//         client_disconnects_send,
//         mut ack,
//         id,
//         counter,
//     } = args;
//     let mut buf = [0u8; SEND_BUF];
//     let mut quota = RECV_BUF_SIZE;
//     loop {
//         let size = match client.read(&mut buf).await {
//             Ok(r) => r,
//             Err(_) => {
//                 let mut server_stream = server.lock().await;
//                 if (Message::Disconnect(id).encode(&mut *server_stream).await).is_err() {
//                     server_stream_error.notify_waiters();
//                 };
//                 client_disconnects_send.send(id).await.unwrap();
//                 return;
//             }
//         };
//         while quota < size {
//             match ack.recv().await {
//                 Some(n) => quota += n,
//                 None => {
//                     // This means the server recv thread isn't alive anymore, which means
//                     // the connection is severed.
//                     return;
//                 }
//             }
//         }
//         if let Some(size) = NonZeroU32::new(size as u32) {
//             limiter
//                 .until_n_ready(size)
//                 .await
//                 .expect("Buffer size should be < max bandwidth");
//         } else {
//             let mut server_stream = server.lock().await;
//             if (Message::Disconnect(id).encode(&mut *server_stream).await).is_err() {
//                 server_stream_error.notify_waiters();
//             };
//             client_disconnects_send.send(id).await.unwrap();
//             return;
//         };
//         quota -= size;
//         let mut server = server.lock().await;
//         if Message::Data(id, &buf[..size])
//             .encode(&mut *server)
//             .await
//             .is_err()
//         {
//             server_stream_error.notify_waiters();
//             return;
//         };
//     }
// }

#[allow(clippy::type_complexity)]
fn get_random_prefix(
    map: &HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>,
) -> Vec<u8> {
    use wordlist::WORD_LIST;
    let mut s = Vec::new();
    while map.contains_key(&s) {
        let mut rng = rand::rng();
        let first = WORD_LIST.choose(&mut rng).unwrap();
        let second = WORD_LIST.choose(&mut rng).unwrap();
        s = format!("{first}-{second}").into_bytes();
    }
    s
}

#[derive(Parser, Debug)]
struct Args {
    base_domain: String,
    #[arg(long, default_value = "0.0.0.0:25565")]
    client_socket_addr: String,
    #[arg(long, default_value = "0.0.0.0:25564")]
    server_socket_addr: String,

    #[arg(long, default_value_t=u64::MAX)]
    max_network: u64,
}

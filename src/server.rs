use governor::Quota;
use mineshare::*;
use rand::seq::IndexedRandom;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::select;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use clap::Parser;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    async_main().await
}

async fn async_main() {
    let args = Args::parse();
    let base_domain: Arc<str> = Arc::from(args.base_domain);
    let map = Arc::new(RwLock::new(HashMap::<
        Vec<u8>,
        mpsc::Sender<(TcpStream, Vec<u8>)>,
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
    let base_domain2 = base_domain.clone();
    let map2 = map.clone();
    tokio::task::spawn(async move {
        let map = map2;
        loop {
            let (stream, addr) = server_listener
                .accept()
                .await
                .expect("Accepting client shouldn't fail");
            let map = map.clone();
            let base_domain = base_domain2.clone();
            tokio::task::spawn(server_handler(stream, addr, map, base_domain));
        }
    });

    // TODO spawn client listener

    tokio::signal::ctrl_c().await.unwrap();
}

#[allow(clippy::type_complexity)]
async fn server_handler(
    mut stream: TcpStream,
    addr: SocketAddr,
    map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>)>>>>,
    base_domain: Arc<str>,
) {
    let mut recv = {
        let mut locked = map.write().await;
        let mut prefix = get_random_prefix(&locked);
        prefix.push(b'.');
        prefix.extend(base_domain.as_bytes());
        let url = prefix;
        if Message::DomainDecided(&url)
            .encode(&mut stream)
            .await
            .is_err()
        {
            return;
        }
        let (send, recv) = mpsc::channel::<(TcpStream, Vec<u8>)>(10);
        locked.insert(url, send);
        recv
    };

    let (server_recv, server_send) = stream.into_split();
    let server_send = Arc::new(Mutex::new(server_send));
    let (client_send, client_recv) = mpsc::channel::<Client>(10);
    let (disconnect_toserver_send, disconnect_toserver_recv) = mpsc::channel::<u64>(10);
    let (disconnect_toclient_send, mut disconnect_toclient_recv) = mpsc::channel::<u64>(10);

    let server_err_notify = Arc::new(Notify::new());

    let server_joinhandle = tokio::task::spawn(handle_server_to_client(ServerToClientArgs {
        server_stream: server_recv,
        server_send: server_send.clone(),
        clients: client_recv,
        client_disconnects_send: disconnect_toclient_send,
        client_disconnects_recv: disconnect_toserver_recv,
    }));

    let mut id = 0;
    let mut client_joins: HashMap<u64, JoinHandle<()>> = HashMap::new();
    loop {
        select! {
            id = disconnect_toclient_recv.recv() => {
                match id {
                    Some(id) => {
                        let task = client_joins.remove(&id).unwrap();
                        task.abort();
                    },
                    None => {
                        client_joins.values().for_each(|h| h.abort());
                        server_joinhandle.abort();
                        return;
                    },
                }
            }
            _err = server_err_notify.notified() => {
                // This means server connection is/was lost for some reason
                client_joins.values().for_each(|h| h.abort());
                server_joinhandle.abort();
                return;
            }
            c = recv.recv() => {
                id += 1;
                let (client, data) = c.expect("Sender should still be in the hashmap");
                {
                    let mut server_send = server_send.lock().await;
                    if Message::Connect(id).encode(&mut *server_send).await.is_err() {
                        client_joins.values().for_each(|h| h.abort());
                        server_joinhandle.abort();
                        return;
                    }
                    if Message::Data(id, &data).encode(&mut *server_send).await.is_err() {
                        client_joins.values().for_each(|h| h.abort());
                        server_joinhandle.abort();
                        return;
                    }
                }
                let limiter = Arc::new(Limiter::direct(
                    Quota::per_second(NonZeroU32::new(LIMIT_MEGABYTE_PER_SECOND).unwrap())
                        .allow_burst(NonZeroU32::new(LIMIT_BURST).unwrap()),
                ));
                let (ack_send, ack_recv) = mpsc::channel::<usize>(100);
                let (client_read, client_write) = client.into_split();
                client_joins.insert(id, tokio::task::spawn(handle_client_to_server(ClientToServerArgs {
                    server_stream: server_send.clone(),
                    client_stream: client_read,
                    limiter: limiter.clone(),
                    server_stream_error: server_err_notify.clone(),
                    client_disconnects_send: disconnect_toserver_send.clone(),
                    ack: ack_recv,
                    id,
                })));
                if client_send.send(Client {
                    id,
                    writer: client_write,
                    limiter,
                    buf: Box::new([0u8; RECV_BUF_SIZE]),
                    buf_len: 0,
                    ack: ack_send,
                }).await.is_err() {
                    client_joins.values().for_each(|h| h.abort());
                    server_joinhandle.abort();
                    return;
                };
            }
        }
    }
}

struct ClientToServerArgs {
    server_stream: Arc<Mutex<OwnedWriteHalf>>,
    client_stream: OwnedReadHalf,
    limiter: Arc<Limiter>,
    server_stream_error: Arc<Notify>,
    client_disconnects_send: mpsc::Sender<u64>,
    ack: mpsc::Receiver<usize>,
    id: u64,
}

struct Client {
    id: u64,
    writer: OwnedWriteHalf,
    limiter: Arc<Limiter>,
    buf: Box<[u8]>,
    buf_len: usize,
    ack: mpsc::Sender<usize>,
}

struct ServerToClientArgs {
    server_stream: OwnedReadHalf,
    server_send: Arc<Mutex<OwnedWriteHalf>>,
    clients: mpsc::Receiver<Client>,
    client_disconnects_send: mpsc::Sender<u64>,
    client_disconnects_recv: mpsc::Receiver<u64>,
}

async fn handle_server_to_client(args: ServerToClientArgs) {
    let ServerToClientArgs {
        server_stream: mut server,
        server_send,
        clients: mut recv,
        client_disconnects_send,
        mut client_disconnects_recv,
    } = args;

    fn remove_client(
        id: u64,
        clients: &mut Vec<Client>,
        map: &mut HashMap<u64, usize>,
    ) -> Option<()> {
        let index = map.remove(&id)?;
        clients.swap_remove(index);
        let new_client = match clients.get(index) {
            Some(c) => c,
            None => {
                // There's 0 clients in the queue, so no need for swapping
                return Some(());
            }
        };
        // Update the hashmap entry
        *map.get_mut(&new_client.id).unwrap() = index;
        Some(())
    }

    let mut clients: Vec<Client> = Vec::new();
    let mut id_to_index: HashMap<u64, usize> = HashMap::new();

    let mut buf = [0u8; MUX_RECV_BUF_SIZE];
    let mut buf_start: usize = 0;

    let mut timeout_when = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
    let mut timeout_count = 0;

    loop {
        let time = timeout_when;
        let timeout = tokio::time::sleep_until(time);
        select! {
            id = client_disconnects_recv.recv() => {
                match id {
                    Some(n) => {
                        debug_assert!(remove_client(n, &mut clients, &mut id_to_index).is_some());
                    },
                    // If there's no other end, that means the spawner thread is dead
                    // which shouldn't happen
                    None => unreachable!(),
                }
            }
            _ = timeout => {
                if timeout_count > 4 {
                    // Server not responding
                    return;
                }
                timeout_count += 1;
                let mut server = server_send.lock().await;
                if Message::HeartBeat.encode(&mut *server).await.is_err() {
                    return;
                }
            }
            data_amt = server.read(&mut buf[buf_start..]) => {
                match data_amt {
                    Ok(len) => {
                        buf_start += len;
                        loop {
                            let (msg, len) = match Message::decode(&buf[0..buf_start]) {
                                Ok(None) => {
                                    break;
                                },
                                Ok(Some((msg, len))) => {
                                    (msg, len)
                                }
                                Err(_e) => {
                                    // If it errored, that means the server made an error. We can't be sure if
                                    // any further communiations would be valid or not, so we just exit
                                    return;
                                }
                            };

                            match msg {
                                Message::DomainDecided(_) | Message::Connect(_) => {
                                    // We should never get these packets from the server
                                    return;
                                },
                                Message::Disconnect(id) => {
                                    match remove_client(id, &mut clients, &mut id_to_index) {
                                        Some(()) => {
                                            // We confirmed that client is removed from the vec & map
                                            // so we notify the spawner thread to remove the task with
                                            // this ID
                                            client_disconnects_send.send(id).await.unwrap();
                                        },
                                        None => {
                                            // Server sent invalid ID
                                            return;
                                        },
                                    }
                                },
                                Message::Data(id, items) => {
                                    let Some(&index) = id_to_index.get(&id) else {
                                        // Server sent invalid ID
                                        return;
                                    };
                                    let client = &mut clients[index];
                                    let mut buf = &mut client.buf[client.buf_len..];
                                    // If buffer isn't empty and the limiter has enough quota
                                    // then we can just try to send here and now.
                                    if !buf.is_empty() && client.limiter.check_n(NonZeroU32::new(buf.len() as u32).unwrap())
                                        .unwrap().is_ok()
                                    {
                                        if let Ok(wrote) = client.writer.try_write(buf) {
                                            buf.copy_within(wrote.., 0);
                                            client.buf_len -= wrote;
                                            buf = &mut client.buf[client.buf_len..];
                                            let mut server = server_send.lock().await;
                                            if Message::Acknowledged(id, wrote).encode(&mut *server).await.is_err() {
                                                // Server is dead
                                                return;
                                            }
                                        }
                                    }
                                    if let Some(buf) = buf.get_mut(..items.len()) {
                                        buf.copy_from_slice(items);
                                        client.buf_len += items.len();

                                    } else {
                                        // Server sent too much data
                                        return;
                                    }
                                    // See if we can continue writing the new data too
                                    if !buf.is_empty() && client.limiter.check_n(NonZeroU32::new(buf.len() as u32).unwrap())
                                        .unwrap().is_ok()
                                    {
                                        if let Ok(wrote) = client.writer.try_write(buf) {
                                            buf.copy_within(wrote.., 0);
                                            client.buf_len -= wrote;
                                            let mut server = server_send.lock().await;
                                            if Message::Acknowledged(id, wrote).encode(&mut *server).await.is_err() {
                                                // Server is dead
                                                return;
                                            }
                                        }
                                    }
                                },
                                Message::HeartBeat => (),
                                Message::Acknowledged(id, acked) => {
                                    let Some(&index) = id_to_index.get(&id) else {
                                        // Server sent invalid ID
                                        return;
                                    };
                                    // Send ACK to the corresponding client sender so
                                    // it can continue to send data
                                    clients[index].ack.send(acked).await.ok();
                                },
                            }

                            buf.copy_within(len.., 0);
                            buf_start -= len;
                        }
                        timeout_when = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
                    },
                    Err(_e) => {
                        // Server connection failed. Once we return everything will be shut down
                        // by spawner thread.
                        return;
                    },
                }
            }
            client = recv.recv() => {
                match client {
                    Some(c) => {
                        let index = clients.len();
                        let id = c.id;
                        clients.push(c);
                        debug_assert!(id_to_index.insert(id, index).is_none());
                    },
                    None => {
                        // If this is reached, it means the spawner thread died without shutting us down
                        // which should never happen.
                        unreachable!()
                    }
                }
            }
        }
    }
}
async fn handle_client_to_server(args: ClientToServerArgs) {
    let ClientToServerArgs {
        server_stream: server,
        client_stream: mut client,
        limiter,
        server_stream_error,
        client_disconnects_send,
        mut ack,
        id,
    } = args;
    let mut buf = [0u8; SEND_BUF];
    let mut quota = RECV_BUF_SIZE;
    loop {
        let size = match client.read(&mut buf).await {
            Ok(r) => r,
            Err(_) => {
                let mut server_stream = server.lock().await;
                if (Message::Disconnect(id).encode(&mut *server_stream).await).is_err() {
                    server_stream_error.notify_waiters();
                };
                client_disconnects_send.send(id).await.unwrap();
                return;
            }
        };
        if let Some(size) = NonZeroU32::new(size as u32) {
            limiter
                .until_n_ready(size)
                .await
                .expect("Buffer size should be < max bandwidth");
        } else {
            let mut server_stream = server.lock().await;
            if (Message::Disconnect(id).encode(&mut *server_stream).await).is_err() {
                server_stream_error.notify_waiters();
            };
            client_disconnects_send.send(id).await.unwrap();
            return;
        };
        while quota < size {
            match ack.recv().await {
                Some(n) => quota += n,
                None => {
                    // This means the server recv thread isn't alive anymore, which means
                    // the connection is severed.
                    return;
                }
            }
        }
        quota -= size;
        let mut server = server.lock().await;
        if Message::Data(id, &buf[..size])
            .encode(&mut *server)
            .await
            .is_err()
        {
            server_stream_error.notify_waiters();
            return;
        };
    }
}

fn get_random_prefix(map: &HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>)>>) -> Vec<u8> {
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
}

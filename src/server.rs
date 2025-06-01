use clap::Parser;
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
};
use mineshare::*;
use rand::{Rng as _, seq::IndexedRandom as _};
use std::{
    collections::HashMap,
    net::SocketAddr,
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    select,
    sync::{
        Mutex, RwLock, Semaphore,
        mpsc::{self},
        oneshot,
    },
    time::Instant,
};
use tracing::{Instrument as _, info, info_span, trace, warn};

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[tokio::main]
async fn main() {
    async_main().await
}

async fn async_main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let base_domain: Arc<str> = Arc::from(args.base_domain);
    let map = Arc::new(RwLock::new(HashMap::<
        Vec<u8>,
        mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>,
    >::new()));
    let connect_map = Arc::new(Mutex::new(HashMap::new()));

    let global_counter = Arc::new(AtomicU64::new(0));
    server_play_request_handler(&args.server_play_socket_addr, connect_map.clone()).await;
    client_handler(&args.client_socket_addr, map.clone()).await;
    server_initial_handler(
        &args.server_socket_addr,
        map,
        connect_map,
        global_counter.clone(),
        base_domain,
    )
    .await;
    println!("Started listening");
    loop {
        let c = global_counter.load(Ordering::Relaxed);
        if c > args.max_network {
            eprintln!("MAX NETWORK QUOTA EXCEEDED! {c} > {}", args.max_network);
            std::process::exit(1);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[allow(clippy::type_complexity)]
async fn server_initial_handler(
    addr: &str,
    map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>>>,
    cmap: Arc<Mutex<HashMap<u128, oneshot::Sender<(TcpStream, SocketAddr)>>>>,
    counter: Arc<AtomicU64>,
    base_domain: Arc<str>,
) {
    let server_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on server addr `{}`: {e}", addr);
            std::process::exit(1);
        }
    };

    tokio::task::spawn(async move {
        loop {
            let global_counter = counter.clone();
            let (stream, addr) = server_listener
                .accept()
                .await
                .expect("Accepting client shouldn't fail");
            let map = map.clone();
            let base_domain = base_domain.clone();
            let cmap = cmap.clone();
            tokio::task::spawn(server_handler(
                stream,
                addr,
                base_domain,
                map,
                cmap,
                global_counter,
            ));
        }
    });
}

#[allow(clippy::type_complexity)]
async fn client_handler(
    addr: &str,
    map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>>>,
) {
    let client_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on client addr`{}`: {e}", addr);
            std::process::exit(1);
        }
    };
    tokio::task::spawn(async move {
        loop {
            let accepted = client_listener.accept().await.expect("Failed to listen");
            info!("Client {} connected", accepted.1);
            let map = map.clone();
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
                        let domain = match try_parse_init_packet(&data[..cursor], addr) {
                            Ok(Some(v)) => v,
                            Ok(None) => {
                                continue;
                            }
                            Err(_e) => {
                                return Err(())
                            },
                        };
                        let map = map.read().await;
                        if let Some(sender) = map.get(domain) {
                            _ = sender.send((stream, data, addr)).await;
                            return Ok(());
                        } else {
                            warn!("client {addr} tried to connect with invalid URL {}", String::from_utf8_lossy(domain));
                            return Err(());
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
}

#[allow(clippy::type_complexity)]
async fn server_play_request_handler(
    addr: &str,
    cmap: Arc<Mutex<HashMap<u128, oneshot::Sender<(TcpStream, SocketAddr)>>>>,
) {
    let server_play_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "Failed to start listening on server play addr`{}`: {e}",
                addr
            );
            std::process::exit(1);
        }
    };
    tokio::task::spawn(async move {
        loop {
            let (mut accepted, addr) = server_play_listener
                .accept()
                .await
                .expect("Failed to listen");
            let cmap = cmap.clone();
            tokio::task::spawn(async move {
                let id = match tokio::time::timeout(Duration::from_secs(5), accepted.read_u128())
                    .await
                {
                    Ok(Ok(id)) => id,
                    Ok(Err(e)) => {
                        info!("Couldn't read connect ID from {addr}: {e}");
                        return;
                    }
                    Err(_e) => {
                        info!("Couldn't read ID from {addr} due to timeout");
                        return;
                    }
                };
                let mut cmap = cmap.lock().await;
                let Some(sender) = cmap.remove(&id) else {
                    warn!("Server conn request with invalid id: {id}");
                    return;
                };
                _ = sender.send((accepted, addr));
            });
        }
    });
}

#[allow(clippy::type_complexity)]
async fn server_handler(
    mut server_stream: TcpStream,
    addr: SocketAddr,
    base: Arc<str>,
    map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>>>,
    connect_map: Arc<Mutex<HashMap<u128, oneshot::Sender<(TcpStream, SocketAddr)>>>>,
    counter: Arc<AtomicU64>,
) {
    let (mut new_client_recv, prefix) = {
        let mut locked = map.write().await;
        let mut prefix = get_random_prefix(&locked);
        let fut = async {
            prefix.push(b'.');
            prefix.extend(base.as_bytes());
            let arr = (prefix.len() as u64).to_be_bytes();
            server_stream.write_all(&arr).await?;
            server_stream.write_all(&prefix).await?;
            Ok(())
        };

        let res: Result<(), std::io::Error> = fut.await;
        if let Err(e) = res {
            info!("Error sending domain to {addr}: {e}");
            return;
        }
        let url = prefix.clone();
        let (send, recv) = mpsc::channel::<(TcpStream, Vec<u8>, SocketAddr)>(10);
        locked.insert(url, send);
        (recv, prefix)
    };
    let mut timeout_at = Instant::now() + Duration::from_secs(90);
    let mut buf = [0u8; 1024];
    let abort = Arc::new(Semaphore::new(0));
    let closure = async move {
        loop {
            select! {
                _timeout = tokio::time::sleep_until(timeout_at) => {
                    abort.add_permits(Semaphore::MAX_PERMITS);
                    return;
                }
                read = server_stream.read(&mut buf) => {
                    match read {
                        Ok(_) => {
                            timeout_at = Instant::now() + Duration::from_secs(90);
                        },
                        Err(_e) => {
                            abort.add_permits(Semaphore::MAX_PERMITS);
                            return;
                        },
                    }
                }
                recved = new_client_recv.recv() => {
                    let (stream, data, addr) = recved.expect("Sender side should be in the map");
                    let map = connect_map.clone();
                    let m = map.lock().await;
                    let id = get_random_id(&m);
                    drop(m);
                    let counter = counter.clone();
                    if server_stream.write_u128(id).await.is_err() {
                        abort.add_permits(Semaphore::MAX_PERMITS);
                        return;
                    }
                    tokio::task::spawn(handle_connect_two(id, stream, data, addr, counter, map, abort.clone()));
                }
            }
        }
    };
    closure.await;
    info!("Removing {} from map", String::from_utf8_lossy(&prefix));
    let mut locked = map.write().await;
    locked.remove(&prefix);
}

#[allow(clippy::type_complexity)]
async fn handle_connect_two(
    id: u128,
    mut client_stream: TcpStream,
    client_data: Vec<u8>,
    client_addr: SocketAddr,
    counter: Arc<AtomicU64>,
    map: Arc<Mutex<HashMap<u128, oneshot::Sender<(TcpStream, SocketAddr)>>>>,
    abort: Arc<Semaphore>,
) {
    let limiter =
        Limiter::direct(Quota::per_second(LIMIT_MEGABYTE_PER_SECOND).allow_burst(LIMIT_BURST));
    let mut locked = map.lock().await;
    let (send, recv) = oneshot::channel();
    locked.insert(id, send);
    drop(locked);
    let res = tokio::time::timeout(Duration::from_secs(10), recv);
    let (server_stream, socketaddr) = match res.await {
        Ok(s) => s.expect("Should be in map"),
        Err(_e) => {
            let mut locked = map.lock().await;
            locked.remove(&id);
            _ = client_stream.shutdown().await;
            return;
        }
    };
    handle_duplex(
        client_stream,
        client_data,
        socketaddr,
        client_addr,
        server_stream,
        limiter,
        counter,
        abort,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_duplex(
    mut client_stream: TcpStream,
    data: Vec<u8>,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    mut server_stream: TcpStream,
    limiter: Limiter,
    counter: Arc<AtomicU64>,
    abort: Arc<Semaphore>,
) {
    let socketfut = async {
        let addr_str = client_addr.to_string();
        let addr_bytes = addr_str.as_bytes();
        let len = addr_bytes.len() as u64;
        server_stream.write_u64(len).await?;
        server_stream.write_all(addr_bytes).await?;
        server_stream.flush().await?;
        Ok::<(), std::io::Error>(())
    };
    if let Err(e) = socketfut.await {
        info!("Failed to write client address {client_addr} to server {server_addr}: {e}");
        // TODO: politely disconnect client
        return;
    }
    if let Err(e) = server_stream.write_all(&data).await {
        info!("Failed to write {client_addr}'s initial packet to server {server_addr}: {e}");
        // TODO: politely disconnect client
        return;
    }
    drop(data);
    if let Err(e) = server_stream.flush().await {
        info!("Failed to write {client_addr}'s initial packet to server {server_addr}: {e}");
        // TODO: politely disconnect client
        return;
    }
    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];
    loop {
        select! {
            _aborted = abort.acquire() => {
                // If we get here, it means aborted.
                _ = client_stream.shutdown().await;
                _ = server_stream.shutdown().await;
                return;
            }
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
                        if let Err(e) = server_stream.flush().await {
                            info!("Error when writing to server {server_addr} by client {client_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = client_stream.shutdown().await;
                            _ = server_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr} connected to {server_addr}: {e}");
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
                        if let Err(e) = client_stream.write_all(&buf2[..amt]).await {
                            info!("Error when writing to client {client_addr} by server {server_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = server_stream.shutdown().await;
                            _ = client_stream.shutdown().await;
                            return;
                        }
                        if let Err(e) = client_stream.flush().await {
                            info!("Error when writing to client {client_addr} by server {server_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = server_stream.shutdown().await;
                            _ = client_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr} connected to {server_addr}: {e}");
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

#[allow(clippy::type_complexity)]
fn get_random_prefix(
    map: &HashMap<Vec<u8>, mpsc::Sender<(TcpStream, Vec<u8>, SocketAddr)>>,
) -> Vec<u8> {
    use wordlist::WORD_LIST;
    let mut rng = rand::rng();
    let first = WORD_LIST.choose(&mut rng).unwrap();
    let second = WORD_LIST.choose(&mut rng).unwrap();
    let mut s = format!("{first}-{second}").into_bytes();
    while map.contains_key(&s) {
        let first = WORD_LIST.choose(&mut rng).unwrap();
        let second = WORD_LIST.choose(&mut rng).unwrap();
        s = format!("{first}-{second}").into_bytes();
    }
    s
}

#[allow(clippy::type_complexity)]
fn get_random_id<T>(map: &HashMap<u128, oneshot::Sender<T>>) -> u128 {
    let mut rng = rand::rng();
    let mut n: u128 = rng.random();
    while map.contains_key(&n) {
        n = rng.random();
    }
    n
}

#[derive(Parser, Debug)]
struct Args {
    base_domain: String,
    #[arg(long, default_value = "0.0.0.0:25565")]
    client_socket_addr: String,
    #[arg(long, default_value = "0.0.0.0:25564")]
    server_socket_addr: String,
    #[arg(long, default_value = "0.0.0.0:25563")]
    server_play_socket_addr: String,

    #[arg(long, default_value_t=u64::MAX)]
    max_network: u64,
}

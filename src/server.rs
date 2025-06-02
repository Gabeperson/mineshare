#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]

use clap::Parser;
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
};
use mineshare::{LIMIT_BURST, LIMIT_MEGABYTE_PER_SECOND, try_parse_init_packet, varint, wordlist};
use rand::{Rng as _, seq::IndexedRandom as _};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufWriter},
    net::{TcpListener, TcpStream},
    select,
    sync::{
        RwLock, Semaphore,
        mpsc::{self},
        oneshot,
    },
    time::Instant,
};
use tracing::{Level, debug, error, info, instrument, trace, warn};

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[tokio::main]
async fn main() {
    async_main().await;
}

async fn async_main() {
    tracing_subscriber::fmt::fmt()
        .with_max_level(Level::DEBUG)
        .init();
    let args = Args::parse();
    let quota = Quota::per_second(args.rate_limit_recharge).allow_burst(args.rate_limit_burst);
    let router = Router::new(&args.base_domain, quota);

    let global_counter = Arc::new(AtomicU64::new(0));
    server_initial_handler(
        &args.server_socket_addr,
        global_counter.clone(),
        router.clone(),
    )
    .await;
    client_handler(&args.client_socket_addr, router.clone()).await;
    server_play_request_handler(&args.server_play_socket_addr, router.clone()).await;
    info!("Started listening");
    // Megabytes
    let max_network = args.max_network * 1024 * 1024;
    let every = Duration::from_secs(1800);
    let mut check = Instant::now() + every;
    loop {
        let c = global_counter.load(Ordering::Relaxed);
        if c > max_network {
            error!(
                "MAX NETWORK QUOTA EXCEEDED! {}MiB > {}MiB",
                c / 1024 / 1024,
                args.max_network
            );
            std::process::exit(1);
        }
        if Instant::now() > check {
            debug!(
                "Network usage: {}MiB/{}MiB used/max",
                c / 1024 / 1024,
                args.max_network
            );
            check = Instant::now() + every;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[instrument(skip_all)]
async fn server_initial_handler(addr: &str, counter: Arc<AtomicU64>, router: Router) {
    let server_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on server addr `{addr}`: {e}");
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
            info!("Received server spawn request from {addr}");
            let router = router.clone();
            tokio::task::spawn(server_handler(stream, addr, global_counter, router));
        }
    });
    info!("Successfully setup server initial connection handler");
}

#[instrument(skip_all)]
async fn client_handler(addr: &str, router: Router) {
    let client_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on client addr `{addr}`: {e}");
            std::process::exit(1);
        }
    };
    tokio::task::spawn(async move {
        loop {
            let accepted = client_listener.accept().await.expect("Failed to listen");
            info!("Received client connection request from {}", accepted.1);
            let router = router.clone();
            tokio::task::spawn(async move {
                let (mut stream, addr) = accepted;
                let mut data = vec![0u8; 2048];
                let mut cursor = 0;
                if router.check_ratelimit(addr.ip()).await {
                    _ = DisconnectMessage("You have been rate limited")
                        .encode(&mut stream)
                        .await;
                    warn!("Rate Limit: {}", addr.ip());
                    return;
                }
                let fut = async {
                    debug!("Parsing host address from {addr}");
                    loop {
                        match stream.read(&mut data[cursor..]).await {
                            Ok(v) => {
                                cursor += v;
                            }
                            Err(_e) => {
                                info!(
                                    "client {addr} exited without valid initial packet (cannot parse)"
                                );
                                return Err(());
                            }
                        }
                        let domain = match try_parse_init_packet(&data[..cursor], addr) {
                            Ok(Some(v)) => v,
                            Ok(None) => {
                                continue;
                            }
                            Err(_e) => return Err(()),
                        };
                        if let Some(sender) = router.get_domain(domain).await {
                            debug!(
                                "Read domain from {addr}: {}",
                                String::from_utf8_lossy(domain)
                            );
                            _ = sender.send(ClientConn { stream, data, addr }).await;
                            return Ok(());
                        }
                        warn!(
                            "client {addr} tried to connect with invalid URL {}",
                            String::from_utf8_lossy(domain)
                        );
                        return Err(());
                    }
                };
                let timeout = tokio::time::timeout(Duration::from_secs(15), fut);
                match timeout.await {
                    Ok(Ok(())) => {
                        info!("client {addr} sent to handler thread");
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
    info!("Successfully setup client handler");
}

#[instrument(skip_all)]
async fn server_play_request_handler(addr: &str, router: Router) {
    let server_play_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on server play addr `{addr}`: {e}",);
            std::process::exit(1);
        }
    };
    tokio::task::spawn(async move {
        loop {
            let (mut accepted, addr) = server_play_listener
                .accept()
                .await
                .expect("Failed to listen");
            debug!("Received server PLAY request from {addr}");
            let router = router.clone();
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
                info!("Server play connection with id {id} connected");
                let Some(sender) = router.get_id(id).await else {
                    warn!("Server conn request with invalid id: {id}");
                    return;
                };
                _ = sender.send(ServerPlayConn {
                    stream: accepted,
                    addr,
                });
            });
        }
    });
}

#[instrument(skip_all)]
async fn server_handler(
    mut server_stream: TcpStream,
    addr: SocketAddr,
    counter: Arc<AtomicU64>,
    router: Router,
) {
    let (mut new_client_recv, prefix) = {
        let (send, recv) = mpsc::channel(10);
        let fut = async {
            let prefix = router.add_server(send).await;
            let arr = (prefix.len() as u64).to_be_bytes();
            server_stream.write_all(&arr).await?;
            server_stream.write_all(&prefix).await?;
            Ok(prefix)
        };
        let res: Result<Vec<u8>, std::io::Error> = fut.await;
        let prefix = match res {
            Ok(v) => v,
            Err(e) => {
                info!("Error sending domain to {addr}: {e}");
                return;
            }
        };
        (recv, prefix)
    };
    let mut timeout_at = Instant::now() + Duration::from_secs(90);
    let mut buf = [0u8; 1024];
    let abort = Abort::new();
    let router2 = router.clone();
    debug!("Setup server_handler for {addr}");
    let closure = async move {
        loop {
            select! {
                _abort = abort.wait() => {
                    debug!("Server_handler for {addr} aborting due to receiving signal");
                    return;
                }
                _timeout = tokio::time::sleep_until(timeout_at) => {
                    abort.abort();
                    return;
                }
                read = server_stream.read(&mut buf) => {
                    match read {
                        Ok(_) => {
                            timeout_at = Instant::now() + Duration::from_secs(90);
                        },
                        Err(_e) => {
                            abort.abort();
                            return;
                        },
                    }
                }
                recved = new_client_recv.recv() => {
                    debug!("Server {addr} received new client");
                    let clientconn = recved.expect("Sender side should be in the map");
                    let counter = counter.clone();
                    let (send, recv) = oneshot::channel();
                    let id = router2.add_random_id(send).await;
                    if server_stream.write_u128(id).await.is_err() {
                        abort.abort();
                        return;
                    }
                    tokio::task::spawn(handle_connect_two(id, recv, clientconn, counter, router2.clone(), abort.clone()));
                }
            }
        }
    };
    closure.await;
    info!("Removing {} from map", String::from_utf8_lossy(&prefix));
    router.remove_prefix(&prefix).await;
}

#[instrument(skip_all)]
async fn handle_connect_two(
    id: u128,
    recv: oneshot::Receiver<ServerPlayConn>,
    mut client_conn: ClientConn,
    counter: Arc<AtomicU64>,
    router: Router,
    abort: Abort,
) {
    let limiter =
        Limiter::direct(Quota::per_second(LIMIT_MEGABYTE_PER_SECOND).allow_burst(LIMIT_BURST));
    let res = tokio::time::timeout(Duration::from_secs(10), recv);
    let ServerPlayConn {
        stream: server_stream,
        addr: socketaddr,
    } = match res.await {
        Ok(s) => s.expect("Should be in map"),
        Err(_e) => {
            router.remove_id(id).await;
            _ = DisconnectMessage("Timed out: Server failed to accept connection")
                .encode(&mut client_conn.stream)
                .await;
            _ = client_conn.stream.shutdown().await;
            return;
        }
    };
    handle_duplex(
        client_conn,
        socketaddr,
        server_stream,
        limiter,
        counter,
        abort,
    )
    .await;
}

#[instrument(skip_all)]
async fn handle_duplex(
    client_conn: ClientConn,
    server_addr: SocketAddr,
    mut server_stream: TcpStream,
    limiter: Limiter,
    counter: Arc<AtomicU64>,
    abort: Abort,
) {
    let ClientConn {
        stream: mut client_stream,
        data,
        addr: client_addr,
    } = client_conn;
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
        _ = DisconnectMessage("Failed to send socketaddr to server")
            .encode(&mut client_stream)
            .await;
        return;
    }
    if let Err(e) = server_stream.write_all(&data).await {
        info!("Failed to write {client_addr}'s initial packet to server {server_addr}: {e}");
        _ = DisconnectMessage("Failed to send init packet to server")
            .encode(&mut client_stream)
            .await;
        return;
    }
    drop(data);
    if let Err(e) = server_stream.flush().await {
        info!("Failed to write {client_addr}'s initial packet to server {server_addr}: {e}");
        _ = DisconnectMessage("Failed to send init packet to server")
            .encode(&mut client_stream)
            .await;
        return;
    }
    info!("Connected client {client_addr} with {server_addr}. Starting bidirectional proxying");
    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];
    loop {
        select! {
            _aborted = abort.wait() => {
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

#[derive(Debug, Clone)]
struct Router {
    base: Arc<str>,
    domain_map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<ClientConn>>>>,
    play_connect_map: Arc<RwLock<HashMap<u128, oneshot::Sender<ServerPlayConn>>>>,
    rate_limits: Arc<RwLock<HashMap<IpAddr, Limiter>>>,
    quota: Quota,
}

impl Router {
    fn new(base: &str, quota: Quota) -> Self {
        Self {
            base: Arc::from(base),
            domain_map: Arc::default(),
            play_connect_map: Arc::default(),
            rate_limits: Arc::default(),
            quota,
        }
    }
    /// Check if IP is rate limited
    async fn check_ratelimit(&self, ip: IpAddr) -> bool {
        trace!("Checking ratelimit for {ip}");
        let locked = self.rate_limits.read().await;
        if let Some(v) = locked.get(&ip) {
            let limited = v.check().is_err();
            trace!("{ip}'s rate limit status: {limited}");
            return limited;
        }
        drop(locked);
        trace!("{ip} never joined before. Adding to map.");
        let mut locked = self.rate_limits.write().await;
        locked.insert(ip, Limiter::direct(self.quota));
        false
    }
    async fn add_server(&self, send: mpsc::Sender<ClientConn>) -> Vec<u8> {
        let mut locked = self.domain_map.write().await;
        let mut prefix = get_random_prefix(&*locked);
        prefix.push('.');
        prefix.push_str(&self.base);
        locked.insert(prefix.clone().into(), send);
        trace!("Adding server with prefix {prefix}");
        prefix.into_bytes()
    }
    async fn add_random_id(&self, send: oneshot::Sender<ServerPlayConn>) -> u128 {
        let mut map = self.play_connect_map.write().await;
        let id = get_random_id(&*map);
        trace!("Adding new random server PLAY ID: {id}");
        map.insert(id, send);
        id
    }
    async fn get_id(&self, id: u128) -> Option<oneshot::Sender<ServerPlayConn>> {
        trace!("Getting server PLAY ID: {id}");
        let mut locked = self.play_connect_map.write().await;
        locked.remove(&id)
    }
    async fn remove_id(&self, id: u128) -> Option<oneshot::Sender<ServerPlayConn>> {
        trace!("Removing server PLAY ID: {id}");
        self.get_id(id).await
    }
    async fn get_domain(&self, prefix: &[u8]) -> Option<mpsc::Sender<ClientConn>> {
        trace!("Getting domain {}", String::from_utf8_lossy(prefix));
        let locked = self.domain_map.read().await;
        locked.get(prefix).cloned()
    }
    async fn remove_prefix(&self, prefix: &[u8]) -> Option<mpsc::Sender<ClientConn>> {
        trace!("Removing prefix {}", String::from_utf8_lossy(prefix));
        let mut locked = self.domain_map.write().await;
        locked.remove(prefix)
    }
}

#[derive(Debug)]
struct ClientConn {
    stream: TcpStream,
    data: Vec<u8>,
    addr: SocketAddr,
}

#[derive(Debug)]
struct ServerPlayConn {
    stream: TcpStream,
    addr: SocketAddr,
}

#[derive(Debug, Clone)]
struct Abort {
    inner: Arc<Semaphore>,
}

impl Abort {
    fn new() -> Self {
        Self {
            inner: Arc::new(Semaphore::new(0)),
        }
    }
    async fn wait(&self) {
        _ = self.inner.acquire().await.expect("Should never be closed");
    }
    fn abort(&self) {
        self.inner.add_permits(Semaphore::MAX_PERMITS);
    }
}

fn get_random_prefix<T>(map: &HashMap<Vec<u8>, mpsc::Sender<T>>) -> String {
    use wordlist::WORD_LIST;
    let mut rng = rand::rng();
    let first = WORD_LIST.choose(&mut rng).unwrap();
    let second = WORD_LIST.choose(&mut rng).unwrap();
    let third = WORD_LIST.choose(&mut rng).unwrap();
    let mut s = format!("{first}-{second}-{third}");
    while map.contains_key(s.as_bytes()) {
        let first = WORD_LIST.choose(&mut rng).unwrap();
        let second = WORD_LIST.choose(&mut rng).unwrap();
        s = format!("{first}-{second}");
    }
    s
}

fn get_random_id<T>(map: &HashMap<u128, oneshot::Sender<T>>) -> u128 {
    let mut rng = rand::rng();
    let mut n: u128 = rng.random();
    while map.contains_key(&n) {
        n = rng.random();
    }
    n
}

#[derive(Debug, Clone)]
struct DisconnectMessage<T: std::fmt::Display>(T);

impl<T: std::fmt::Display> DisconnectMessage<T> {
    async fn encode<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<(), std::io::Error> {
        const DISCONNECT_PACKET_ID: &[u8] = &[0];
        let s = format!(r#"{{"text":"{}"}}"#, self.0);
        trace!("Sending debug message: {s}");
        let s_len = s.len();
        let (arr, len) = varint::encode_varint(s_len as u64);
        let (arr2, len2) =
            varint::encode_varint((DISCONNECT_PACKET_ID.len() + len + s.len()) as u64);
        let mut writer = BufWriter::new(writer);
        writer.write_all(&arr2[..len2]).await?;
        writer.write_all(DISCONNECT_PACKET_ID).await?;
        writer.write_all(&arr[..len]).await?;
        writer.write_all(s.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

#[derive(Parser, Debug)]
struct Args {
    /// The base domain of the proxy.
    /// Ex: If example.com is given, the generated urls will look like word-word-word.example.com
    base_domain: String,
    /// The TCP listener address that clients can use to join the proxied server
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25565")]
    client_socket_addr: String,
    /// The TCP listener address that the server uses to setup the initial connection with the proxy
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25564")]
    server_socket_addr: String,
    /// The TCP listener address that the server uses to setup the duplex connection with the client stream
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25563")]
    server_play_socket_addr: String,
    /// Approximate network transfer limit in MiB. After this limit is reached, the server will
    /// shut down all connections.
    #[arg(long, default_value_t=u64::MAX)]
    max_network: u64,
    /// How quickly the IP rate limit refills. This is effectively how many sustained requests an IP can make per second.
    #[arg(long, default_value = "1")]
    rate_limit_recharge: NonZeroU32,
    /// The burst limit for each IP address. This is the MAX # of burst connections that can be made from each IP in a second.
    #[arg(long, default_value = "10")]
    rate_limit_burst: NonZeroU32,
}

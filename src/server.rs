#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
// I just like large functions :/
#![allow(clippy::too_many_lines)]

use clap::Parser;
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
};
use mineshare::{
    Addr, HELLO_STRING, InitResponse, Message, PROTOCOL_VERSION, ServerHello, StreamHelper as _,
    try_parse_init_packet, varint, wordlist,
};
use rand::{Rng as _, seq::IndexedRandom as _};
use tracing::{info, warn};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    num::{NonZero, NonZeroU32},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufWriter},
    net::{TcpListener, TcpStream},
    select,
    sync::{
        RwLock, Semaphore,
        mpsc::{self},
        oneshot,
    },
    time::Instant,
};

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[tokio::main]
async fn main() {
    async_main().await;
}

async fn async_main() {
    tracing_subscriber::fmt::fmt()
        // .with_max_level(Level::INFO)
        // .with_env_filter("mineshare=info")
        .init();
    let args = Args::parse();
    let quota = Quota::per_second(args.rate_limit_recharge).allow_burst(args.rate_limit_burst);
    let router = Router::new(&args.base_domain, quota);

    let global_counter = Arc::new(AtomicU64::new(0));
    server_initial_handler(
        &args.server_socket_addr,
        global_counter.clone(),
        router.clone(),
        Quota::per_second(args.bandwidth_byte_per_second)
            .allow_burst(args.bandwidth_burst_byte_per_second),
    )
    .await;
    client_handler(&args.client_socket_addr, router.clone()).await;
    server_play_request_handler(
        &args.server_play_socket_addr,
        router.clone(),
        global_counter.clone(),
    )
    .await;
    // Megabytes
    let max_network = args.max_network * 1024 * 1024;
    let every = Duration::from_secs(1800);
    let mut check = Instant::now() + every;
    println!("Successfully setup listeners!");
    loop {
        let c = global_counter.load(Ordering::Relaxed);
        if c > max_network {
            eprintln!(
                "MAX NETWORK QUOTA EXCEEDED! {}MiB > {}MiB",
                c / 1024 / 1024,
                args.max_network
            );
            std::process::exit(1);
        }
        if Instant::now() > check {
            info!(
                "Network usage: {}MiB/{}MiB used/max",
                c / 1024 / 1024,
                args.max_network
            );
            check = Instant::now() + every;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn server_initial_handler(addr: &str, counter: Arc<AtomicU64>, router: Router, quota: Quota) {
    let server_listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on server addr `{addr}`: {e}");
            std::process::exit(1);
        }
    };
    tokio::task::spawn(async move {
        loop {
            let (stream, addr) = server_listener.accept().await.expect("Failed to listen");
            let global_counter = counter.clone();
            let router = router.clone();
            tokio::task::spawn(server_handler(stream, addr, global_counter, router, quota));
        }
    });
}

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
            let router = router.clone();
            tokio::task::spawn(async move {
                let (mut stream, addr) = accepted;
                let mut data = vec![0u8; 2048];
                let mut cursor = 0;
                if router.check_ratelimit(addr.ip()).await {
                    _ = DisconnectMessage("You have been rate limited")
                        .encode(&mut stream)
                        .await;
                    return;
                }
                let fut = async {
                    loop {
                        match stream.read(&mut data[cursor..]).await {
                            Ok(v) => {
                                cursor += v;
                            }
                            Err(_e) => {
                                return Err(());
                            }
                        }
                        let domain = match try_parse_init_packet(&data[..cursor]) {
                            Ok(Some(v)) => v,
                            Ok(None) => {
                                continue;
                            }
                            Err(_e) => return Err(()),
                        };
                        if let Some(sender) = router.get_domain(domain).await {
                            data.truncate(cursor);
                            _ = sender.send(ClientConn { stream, data, addr }).await;
                            return Ok(());
                        }
                        return Err(());
                    }
                };
                let timeout = tokio::time::timeout(Duration::from_secs(5), fut);
                _ = timeout.await;
            });
        }
    });
}

async fn server_play_request_handler(addr: &str, router: Router, counter: Arc<AtomicU64>) {
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
            let router = router.clone();
            let counter = counter.clone();
            tokio::task::spawn(async move {
                let res = tokio::time::timeout(Duration::from_secs(5), accepted.read_u128()).await;
                counter.fetch_add(16, Ordering::Relaxed);
                let Ok(Ok(id)) = res else {
                    return;
                };
                let Some(sender) = router.get_id(id).await else {
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

async fn server_handler(
    mut server_stream: TcpStream,
    addr: SocketAddr,
    counter: Arc<AtomicU64>,
    router: Router,
    quota: Quota,
) {
    if router.check_ratelimit(addr.ip()).await {
        warn!("Rate limited: {}", addr.ip());
        _ = server_stream.shutdown().await;
        return;
    }
    let mut buf = [0u8; 256];
    let Ok(requested) = receive_server_hello(&mut server_stream, &mut buf).await else {
        // This isn't a proper server, it's just sending malformed data or isnt sending data at all.
        // So we just ignore them
        return;
    };
    let (mut new_client_recv, domain) = {
        let (send, recv) = mpsc::channel(10);
        let domain = router.add_server(send, requested).await;
        let wrote = match InitResponse::new(domain.clone(), PROTOCOL_VERSION)
            .encode(&mut server_stream, &mut buf)
            .await
        {
            Ok(v) => v,
            Err(_e) => {
                return;
            }
        };
        counter.fetch_add(wrote as u64, Ordering::Relaxed);
        (recv, domain)
    };
    let heartbeat_response_time = Duration::from_secs(5);
    let between_heartbeats = Duration::from_secs(15);
    let mut timeout_at = Instant::now() + between_heartbeats + heartbeat_response_time;
    let mut send_heartbeat_at = Instant::now() + between_heartbeats;
    let send_timeout = Duration::from_secs(5);
    let abort = Abort::new();
    let router2 = router.clone();
    let mut data = [0u8; 32];
    rand::rng().fill(&mut data);
    info!("Connection: Assigned domain {domain} to {addr}");
    let server: Arc<str> = format!("[{domain} @ {addr}]").into();
    let closure = async move {
        loop {
            select! {
                _abort = abort.wait() => {
                    return;
                }
                _timeout = tokio::time::sleep_until(timeout_at) => {
                    info!("Timed out: {server}");
                    abort.abort();
                    return;
                }
                _heartbeat = tokio::time::sleep_until(send_heartbeat_at) => {
                    let msg = Message::HeartBeat(data);
                    let mut buf = [0u8; 256];
                    match tokio::time::timeout(send_timeout, msg.encode(&mut server_stream, &mut buf)).await {
                        Ok(Ok(l)) => {
                            counter.fetch_add(l as u64, Ordering::Relaxed);
                        },
                        Ok(Err(e)) => {
                            info!("Disconnected: {server}. Cause: {e}");
                            abort.abort();
                            return;
                        }
                        Err(_) => {
                            info!("Disconnected: {server}. Cause: Timeout sending heartbeat");
                            abort.abort();
                            return;
                        },
                    };
                    send_heartbeat_at = Instant::now() + between_heartbeats;
                    timeout_at = Instant::now() + heartbeat_response_time;
                }
                read = server_stream.read(&mut buf) => {
                    let Ok(read) = read else {
                        info!("Disconnected: {server}. Cause: Heartbeat EOF");
                        abort.abort();
                        return;
                    };
                    let data_len = data.len();
                    if read < data_len {
                        let res = tokio::time::timeout(Duration::from_secs(5), server_stream.read_exact(&mut buf[read..data_len])).await;
                        let Ok(Ok(_)) = res else {
                            info!("Timed out: {server}.");
                            abort.abort();
                            return;
                        };
                    }
                    if &buf[..data_len] != &data {
                        info!("Disconnected: {server}. Cause: Invalid heartbeat");
                        abort.abort();
                        return;
                    }
                    send_heartbeat_at = Instant::now() + between_heartbeats;
                    timeout_at = Instant::now() + between_heartbeats + heartbeat_response_time;
                }
                recved = new_client_recv.recv() => {
                    let clientconn = recved.expect("Sender side should be in the map");
                    let counter = counter.clone();
                    let (send, recv) = oneshot::channel();
                    let id = router2.add_random_id(send).await;
                    let msg = Message::NewClient(id);
                    let mut buf = [0u8; 256];
                    match msg.encode(&mut server_stream, &mut buf).await {
                        Ok(l) => {
                            counter.fetch_add(l as u64, Ordering::Relaxed);
                        }
                        Err(_e) => {
                            abort.abort();
                            return;
                        }
                    }
                    tokio::task::spawn(handle_duplex(id, server.clone(), recv, clientconn, counter, router2.clone(), quota, abort.clone()));
                }
            }
        }
    };
    closure.await;
    router.remove_prefix(domain.as_bytes()).await;
}

async fn handle_duplex(
    id: u128,
    server: Arc<str>,
    recv: oneshot::Receiver<ServerPlayConn>,
    mut client_conn: ClientConn,
    counter: Arc<AtomicU64>,
    router: Router,
    quota: Quota,
    abort: Abort,
) {
    let limiter = Limiter::direct(quota);
    let res = tokio::time::timeout(Duration::from_secs(10), recv);

    let ServerPlayConn {
        stream: mut server_stream,
        addr: _socketaddr,
    } = match res.await {
        Ok(Ok(s)) => s,
        Ok(Err(_e)) => {
            router.remove_id(id).await;
            _ = client_conn.stream.shutdown().await;
            return;
        }
        Err(_e) => {
            router.remove_id(id).await;
            _ = DisconnectMessage("Timed out: Server failed to accept connection")
                .encode(&mut client_conn.stream)
                .await;
            _ = client_conn.stream.shutdown().await;
            return;
        }
    };
    let ClientConn {
        stream: mut client_stream,
        data,
        addr: client_addr,
    } = client_conn;
    let mut buf = vec![0u8; 128];
    match Addr(client_addr).encode(&mut server_stream, &mut buf).await {
        Ok(len) => {
            counter.fetch_add(len as u64, Ordering::Relaxed);
        }
        Err(_e) => {
            _ = DisconnectMessage("Failed to send socketaddr to server")
                .encode(&mut client_stream)
                .await;
            return;
        }
    }
    drop(buf);
    if let Err(_e) = server_stream.write_all(&data).await {
        _ = DisconnectMessage("Failed to send init packet to server")
            .encode(&mut client_stream)
            .await;
        return;
    }
    drop(data);
    if let Err(_e) = server_stream.flush().await {
        _ = DisconnectMessage("Failed to send init packet to server")
            .encode(&mut client_stream)
            .await;
        return;
    }
    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];
    info!("Client {client_addr} connected to {server}");
    let fut = async move {
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
                            if let Err(_e) = server_stream.write_all(&buf1[..amt]).await {
                                // If one of the connections errors, we should abort the other one too.
                                // We ignore the returned results because there's nothing we can do if the disconnection fails.
                                _ = client_stream.shutdown().await;
                                _ = server_stream.shutdown().await;
                                return;
                            }
                            if let Err(_e) = server_stream.flush().await {
                                // If one of the connections errors, we should abort the other one too.
                                // We ignore the returned results because there's nothing we can do if the disconnection fails.
                                _ = client_stream.shutdown().await;
                                _ = server_stream.shutdown().await;
                                return;
                            }
                        },
                        Err(_e) => {
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
                            if let Err(_e) = client_stream.write_all(&buf2[..amt]).await {
                                // If one of the connections errors, we should abort the other one too.
                                // We ignore the returned results because there's nothing we can do if the disconnection fails.
                                _ = server_stream.shutdown().await;
                                _ = client_stream.shutdown().await;
                                return;
                            }
                            if let Err(_e) = client_stream.flush().await {
                                // If one of the connections errors, we should abort the other one too.
                                // We ignore the returned results because there's nothing we can do if the disconnection fails.
                                _ = server_stream.shutdown().await;
                                _ = client_stream.shutdown().await;
                                return;
                            }
                        },
                        Err(_e) => {
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
    };
    fut.await;
    info!("Client {client_addr} disconnected from {server}");
}

#[derive(Debug, Clone)]
struct Router {
    base: Arc<str>,
    domain_map: Arc<RwLock<HashMap<Vec<u8>, mpsc::Sender<ClientConn>>>>,
    play_connect_map: Arc<RwLock<HashMap<u128, oneshot::Sender<ServerPlayConn>>>>,
    rate_limits: Arc<RwLock<HashMap<IpAddr, Limiter>>>,
    ip_quota: Quota,
}

impl Router {
    fn new(base: &str, ip_quota: Quota) -> Self {
        Self {
            base: Arc::from(base),
            domain_map: Arc::default(),
            play_connect_map: Arc::default(),
            rate_limits: Arc::default(),
            ip_quota,
        }
    }
    /// Check if IP is rate limited
    async fn check_ratelimit(&self, ip: IpAddr) -> bool {
        let locked = self.rate_limits.read().await;
        if let Some(v) = locked.get(&ip) {
            let limited = v.check().is_err();
            return limited;
        }
        drop(locked);
        let mut locked = self.rate_limits.write().await;
        locked.insert(ip, Limiter::direct(self.ip_quota));
        false
    }
    async fn add_server(&self, send: mpsc::Sender<ClientConn>, req: Option<&str>) -> String {
        let mut locked = self.domain_map.write().await;
        let domain = 'blck: {
            if let Some(s) = req {
                if s.ends_with(&*self.base) && !locked.contains_key(s.as_bytes()) {
                    break 'blck s.to_string();
                }
            }
            let mut domain = get_random_prefix(&*locked);
            domain.push('.');
            domain.push_str(&self.base);
            domain
        };
        locked.insert(domain.clone().into(), send);
        domain
    }
    async fn add_random_id(&self, send: oneshot::Sender<ServerPlayConn>) -> u128 {
        let mut map = self.play_connect_map.write().await;
        let id = get_random_id(&*map);
        map.insert(id, send);
        id
    }
    async fn get_id(&self, id: u128) -> Option<oneshot::Sender<ServerPlayConn>> {
        let mut locked = self.play_connect_map.write().await;
        locked.remove(&id)
    }
    async fn remove_id(&self, id: u128) -> Option<oneshot::Sender<ServerPlayConn>> {
        self.get_id(id).await
    }
    async fn get_domain(&self, prefix: &[u8]) -> Option<mpsc::Sender<ClientConn>> {
        let locked = self.domain_map.read().await;
        locked.get(prefix).cloned()
    }
    async fn remove_prefix(&self, prefix: &[u8]) -> Option<mpsc::Sender<ClientConn>> {
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

/// Returns requested domain
async fn receive_server_hello<'a, S: AsyncRead + Unpin + Send>(
    stream: &mut S,
    buf: &'a mut [u8],
) -> Result<Option<&'a str>, ()> {
    let fut = ServerHello::decode(stream, buf);
    let fut = tokio::time::timeout(Duration::from_secs(5), fut);
    match fut.await {
        Ok(Ok(serverhello)) if serverhello.hello_string == HELLO_STRING => {
            Ok(serverhello.requested_domain)
        }
        _ => Err(()),
    }
}

#[derive(Debug, Clone)]
struct DisconnectMessage<T: std::fmt::Display>(T);

impl<T: std::fmt::Display> DisconnectMessage<T> {
    async fn encode<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<(), std::io::Error> {
        let res = async {
            const DISCONNECT_PACKET_ID: &[u8] = &[0];
            let s = format!(r#"{{"text":"{}"}}"#, self.0);
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
        };
        tokio::time::timeout(Duration::from_secs(5), res)
            .await
            .map_err(|_e| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Sending Disconnect took too long",
                )
            })?
    }
}

#[derive(Parser, Debug)]
struct Args {
    /// The base domain of the proxy.
    /// Ex: If example.com is given, the generated urls will look like word-word-word.example.com
    base_domain: String,
    /// The TCP listener IP:PORT that clients can use to join the proxied server
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25565")]
    client_socket_addr: String,
    /// The TCP listener IP address ONLY that the server uses to setup the initial connection with the proxy
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25563")]
    server_socket_addr: String,
    /// The TCP listener IP:PORT that the server uses to setup the duplex connection with the client stream
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25564")]
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
    /// How much bandwidth each connection between client and server should be allowed to have, in B/s.
    /// AKA how much data should each Minecraft connection be allowed to send & receive per second
    #[arg(long, default_value_t = const { NonZero::new(128*1024).unwrap() })]
    bandwidth_byte_per_second: NonZero<u32>,
    /// How much *burst* bandwidth each connection between client and server should be allowed to have, in B/s.
    /// AKA the maximum burst that the `bandwidth_megabyte_per_second` should have.
    #[arg(long, default_value_t = const { NonZero::new(256*1024).unwrap() })]
    bandwidth_burst_byte_per_second: NonZero<u32>,
}

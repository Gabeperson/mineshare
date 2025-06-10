#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
// I just like large functions :/
#![allow(clippy::too_many_lines)]

use clap::Parser;
use ed25519_dalek::{SigningKey, VerifyingKey};
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
};
use mineshare::{
    Addr, BincodeAsync as _, DomainAndPubKey, Message, PROTOCOL_VERSION, ServerHello,
    dhauth::AuthenticatorProxy, try_parse_init_packet, varint, wordlist,
};
use rand::{Rng as _, seq::IndexedRandom as _};
use rustls_acme::{AcmeConfig, caches::DirCache};
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
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tracing::{Level, error, info, trace, warn};

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[tokio::main]
async fn main() {
    async_main().await;
}

async fn async_main() {
    let mut secret_key_bytes = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
    rand::rng().fill(&mut secret_key_bytes);
    let alice_signing_key = SigningKey::from_bytes(&secret_key_bytes);
    let alice_verify_key = alice_signing_key.verifying_key();
    tracing_subscriber::fmt::fmt()
        .with_max_level(Level::INFO)
        .with_env_filter("mineshare=info")
        .init();
    let args = Args::parse();
    let quota = Quota::per_second(args.rate_limit_recharge).allow_burst(args.rate_limit_burst);
    let router = Router::new(&args.base_domain, quota);

    let global_counter = Arc::new(AtomicU64::new(0));
    server_initial_handler(
        &args.server_socket_addr,
        global_counter.clone(),
        router.clone(),
        args.email,
        args.prefix,
        args.prod,
        alice_verify_key,
        Quota::per_second(args.bandwidth_megabyte_per_second)
            .allow_burst(args.bandwidth_burst_megabyte_per_second),
    )
    .await;
    client_handler(&args.client_socket_addr, router.clone()).await;
    server_play_request_handler(
        &args.server_play_socket_addr,
        router.clone(),
        alice_signing_key,
        global_counter.clone(),
    )
    .await;
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
async fn server_initial_handler(
    addr: &str,
    counter: Arc<AtomicU64>,
    router: Router,
    email: String,
    prefix: String,
    prod: bool,
    verifying_key: VerifyingKey,
    quota: Quota,
) {
    let addr = format!("{addr}:443");
    let server_listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to start listening on server addr `{addr}`: {e}");
            std::process::exit(1);
        }
    };

    let tcp_incoming = TcpListenerStream::new(server_listener);

    let mut server_listener = AcmeConfig::new(vec![format!("{prefix}.{}", router.base)])
        .contact_push(format!("mailto:{email}"))
        .cache(DirCache::new("tls_certs"))
        .directory_lets_encrypt(prod)
        .tokio_incoming(tcp_incoming, vec![b"mineshare".into()]);

    tokio::task::spawn(async move {
        while let Some(tls) = server_listener.next().await {
            let global_counter = counter.clone();
            let stream = tls.expect("Shouldn't fail to accept connection");
            let router = router.clone();
            let Ok(addr) = stream.get_ref().get_ref().0.get_ref().peer_addr() else {
                error!("Failed to get peer adrr: {addr}");
                continue;
            };
            info!("Received server spawn request from {addr}");
            tokio::task::spawn(server_handler(
                stream,
                addr,
                global_counter,
                router,
                verifying_key,
                quota,
            ));
        }
    });
    info!("Successfully setup server initial connection handler");
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
                            info!(
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
                let timeout = tokio::time::timeout(Duration::from_secs(5), fut);
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

async fn server_play_request_handler(
    addr: &str,
    router: Router,
    pkey: SigningKey,
    counter: Arc<AtomicU64>,
) {
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
            let pkey = pkey.clone();
            let counter = counter.clone();
            tokio::task::spawn(async move {
                let auth = AuthenticatorProxy {
                    inner: &mut accepted,
                    alice_private_sign_key: pkey,
                    counter: &counter,
                };
                let id = match tokio::time::timeout(Duration::from_secs(5), auth.get_id()).await {
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

async fn server_handler<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut server_stream: S,
    addr: SocketAddr,
    counter: Arc<AtomicU64>,
    router: Router,
    verifying_key: VerifyingKey,
    quota: Quota,
) {
    let mut decode_buf = [0u8; 512];
    if receive_server_hello(&mut server_stream, &mut decode_buf)
        .await
        .is_err()
    {
        // This isn't a proper server, it's just sending malformed data or isnt sending data at all.
        // So we just ignore them
        return;
    }
    let (mut new_client_recv, prefix) = {
        let (send, recv) = mpsc::channel(10);
        let prefix = router.add_server(send).await;
        let wrote =
            match DomainAndPubKey::new(prefix.clone(), verifying_key.to_bytes(), PROTOCOL_VERSION)
                .encode(&mut server_stream, &mut decode_buf)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    info!("Error sending domain to {addr}: {e}");
                    return;
                }
            };
        counter.fetch_add(wrote as u64, Ordering::Relaxed);
        (recv, prefix)
    };
    let timeout_duration = Duration::from_secs(90);
    let heartbeat_time = Duration::from_secs(5);
    let send_timeout = Duration::from_secs(5);
    let mut timeout_at = Instant::now() + timeout_duration;
    let mut hb_read = 0;
    let abort = Abort::new();
    let router2 = router.clone();
    info!("Setup server_handler for {addr} (Passed server hello)");
    let mut data = [0u8; 32];
    let mut send_heartbeat_at = Instant::now() + heartbeat_time;
    rand::rng().fill(&mut data);
    let closure = async move {
        loop {
            select! {
                _abort = abort.wait() => {
                    info!("Server_handler for {addr} aborting due to receiving signal");
                    return;
                }
                _timeout = tokio::time::sleep_until(timeout_at) => {
                    abort.abort();
                    return;
                }
                _heartbeat = tokio::time::sleep_until(send_heartbeat_at) => {
                    let msg = Message::HeartBeat(data);
                    let mut buf = [0u8; 64];
                    let res = match tokio::time::timeout(send_timeout, msg.encode(&mut server_stream, &mut buf)).await {
                        Ok(r) => r,
                        Err(_e) => {
                            warn!("Server {addr} did not receive heartbeat in time");
                            abort.abort();
                            return;
                        },
                    };
                    match res {
                        Ok(l) => {
                            counter.fetch_add(l as u64, Ordering::Relaxed);
                        }
                        Err(_e) => {
                            abort.abort();
                            return;
                        },
                    }
                    send_heartbeat_at = Instant::now() + heartbeat_time;
                }
                read = server_stream.read(&mut decode_buf[hb_read..]) => {
                    // This is intentionally written to be nonblocking so we don't need to care about timeouts
                    'blck: {
                        match read {

                            Ok(0) => {
                                abort.abort();
                                _ = server_stream.shutdown();
                                return;
                            },
                            Ok(amt) => {
                                const PACKET_LENGTH_BYTES: usize = 4;
                                hb_read += amt;
                                if hb_read < PACKET_LENGTH_BYTES {
                                    // Haven't read enough to read packet length
                                    break 'blck;
                                }
                                const { assert!(u32::BITS / 8 == PACKET_LENGTH_BYTES as u32) }
                                let len: u32 = u32::from_be_bytes(decode_buf[..PACKET_LENGTH_BYTES].try_into().unwrap());
                                if hb_read- PACKET_LENGTH_BYTES < len as usize {
                                    // Haven't read full packet yet
                                    break 'blck;
                                }
                                let (decoded, _len) = match bincode::decode_from_slice::<Message, _>(&decode_buf[PACKET_LENGTH_BYTES..hb_read], bincode::config::standard()) {
                                    Ok(m) => m,
                                    Err(e) => {
                                        warn!("Invalid packet send by server {addr}: {e}");
                                        abort.abort();
                                        return;
                                    },
                                };
                                let Message::HeartBeatEcho(received_data) = decoded else {
                                    warn!("Invalid packet send by server {addr}: {decoded:?}");
                                    abort.abort();
                                    return;
                                };
                                if received_data != data {
                                    warn!("Invalid echo data server {addr}: {decoded:?}");
                                    abort.abort();
                                    break 'blck;
                                }
                                decode_buf.copy_within(PACKET_LENGTH_BYTES+len as usize.., 0);
                                hb_read -= PACKET_LENGTH_BYTES + len as usize;
                                timeout_at = Instant::now() + timeout_duration;
                                send_heartbeat_at = Instant::now() + heartbeat_time;
                                rand::rng().fill(&mut data);
                            },
                            Err(_e) => {
                                abort.abort();
                                return;
                            },
                        }
                    }
                }
                recved = new_client_recv.recv() => {
                    info!("Server {addr} received new client");
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
                    info!("Send new client msg to {addr}");
                    tokio::task::spawn(handle_connect_two(id, recv, clientconn, counter, router2.clone(), abort.clone(), quota));
                }
            }
        }
    };
    closure.await;
    info!("Removing {} from map", prefix);
    router.remove_prefix(prefix.as_bytes()).await;
}

async fn handle_connect_two(
    id: u128,
    recv: oneshot::Receiver<ServerPlayConn>,
    mut client_conn: ClientConn,
    counter: Arc<AtomicU64>,
    router: Router,
    abort: Abort,
    quota: Quota,
) {
    let limiter = Limiter::direct(quota);
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
    info!("Starting duplex conn. between server {server_addr} and client {client_addr}");
    let mut buf = vec![0u8; 128];
    match Addr(client_addr).encode(&mut server_stream, &mut buf).await {
        Ok(len) => {
            counter.fetch_add(len as u64, Ordering::Relaxed);
        }
        Err(e) => {
            info!("Failed to write client address {client_addr} to server {server_addr}: {e}");
            _ = DisconnectMessage("Failed to send socketaddr to server")
                .encode(&mut client_stream)
                .await;
            return;
        }
    }
    drop(buf);
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
    async fn add_server(&self, send: mpsc::Sender<ClientConn>) -> String {
        let mut locked = self.domain_map.write().await;
        let mut prefix = get_random_prefix(&*locked);
        prefix.push('.');
        prefix.push_str(&self.base);
        locked.insert(prefix.clone().into(), send);
        trace!("Adding server with prefix {prefix}");
        prefix
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

async fn receive_server_hello<S: AsyncRead + Unpin + Send>(
    stream: &mut S,
    buf: &mut [u8],
) -> Result<(), ()> {
    let fut = ServerHello::parse(stream, buf);
    let fut = tokio::time::timeout(Duration::from_secs(5), fut);
    match fut.await {
        Ok(Ok(serverhello)) if serverhello.0 == "mineshare" => Ok(()),
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
    /// The email used for ACME certs
    email: String,
    /// The prefix used by server when connecting to the proxy.
    /// The ACME cert will be reqested with <`prefix`>.<`base_domain`>.
    #[arg(long, default_value = "mc")]
    prefix: String,
    /// The TCP listener IP:PORT that clients can use to join the proxied server
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0:25565")]
    client_socket_addr: String,
    /// The TCP listener IP address ONLY that the server uses to setup the initial connection with the proxy
    /// This uses port 443 so it can piggy back off of Lets Encrypt TLS cert for E2E connection between server
    /// and proxy
    /// It is recommended to leave this as the default
    #[arg(long, default_value = "0.0.0.0")]
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
    /// Whether this should connect to the ACTUAL Lets Encrypt production server
    #[arg(long, default_value_t = false)]
    prod: bool,
    /// How much bandwidth each connection between client and server should be allowed to have, in MiB/s.
    /// AKA how much data should each Minecraft connection be allowed to send & receive per second
    #[arg(long, default_value_t = const { NonZero::new(128*1024).unwrap() })]
    bandwidth_megabyte_per_second: NonZero<u32>,
    /// How much *burst* bandwidth each connection between client and server should be allowed to have, in MiB/s.
    /// AKA the maximum burst that the `bandwidth_megabyte_per_second` should have.
    #[arg(long, default_value_t = const { NonZero::new(256*1024).unwrap() })]
    bandwidth_burst_megabyte_per_second: NonZero<u32>,
}

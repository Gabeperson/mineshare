#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use clap::Parser;
use mineshare::{Addr, InitResponse, Message, PROTOCOL_VERSION, ServerHello, StreamHelper as _};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
    sync::Semaphore,
};
use tracing::{Level, error, info};

const DEFAULT_URL: &str = "mc.mineshare.dev";

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

#[tokio::main]
async fn main() {
    async_main().await;
}

async fn async_main() {
    tracing_subscriber::fmt::fmt()
        .with_max_level(Level::INFO)
        .init();
    let args = Args::parse();

    tokio::task::spawn(exp_backoff_loop(args));
    tokio::signal::ctrl_c().await.unwrap();
}

fn minsec(sec: u64) -> String {
    if sec <= 60 {
        format!("{sec}s")
    } else {
        format!("{}m{}s", sec / 60, sec % 60)
    }
}

async fn exp_backoff_loop(args: Args) {
    let mut backoff: u64 = 2;
    let attempts = 8;
    let mut curr_attempt = 1;
    loop {
        if let Ok(()) = exp_backoff_try(&args).await {
            backoff = 2;
            curr_attempt = 1;
        } else {
            backoff *= 2;
            curr_attempt += 1;
        }
        if curr_attempt > attempts {
            eprintln!("Failed to establish connection with proxy server. Aborting.");
            std::process::exit(1);
        }

        let update_interval = 5;
        let mut remaining = backoff;
        while remaining > 0 {
            let sleep_for = remaining.min(update_interval);
            eprintln!("Server connection lost. Retrying in {}", minsec(remaining));
            tokio::time::sleep(Duration::from_secs(sleep_for)).await;
            remaining -= sleep_for;
        }
    }
}

async fn exp_backoff_try(args: &Args) -> Result<(), ()> {
    info!("Starting proxy connection");
    let mut proxy_conn = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&format!(
            "{}:{}",
            &args.proxy_server, &args.proxy_server_init_port
        )),
    )
    .await
    {
        Ok(Ok(l)) => l,
        Err(_) => {
            error!(
                "Failed to connect to proxy server `{}`: Connection timed out",
                args.proxy_server
            );
            return Err(());
        }
        Ok(Err(e)) => {
            error!(
                "Failed to connect to proxy server `{}`: {e}",
                args.proxy_server
            );
            return Err(());
        }
    };
    let mut v = vec![0u8; 512];
    if let Err(e) = ServerHello::new(args.request_domain.as_deref())
        .encode(&mut proxy_conn, &mut v)
        .await
    {
        error!("Error sending server hello to proxy server: {e}");
    }
    info!("Sent initial hello to proxy server");
    info!("Fetching Url");
    let InitResponse {
        domain,
        protocol_version,
    } = match InitResponse::decode(&mut proxy_conn, &mut v).await {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to fetch domain from proxy server: {e}");
            std::process::exit(1);
        }
    };
    if protocol_version != PROTOCOL_VERSION {
        if protocol_version < PROTOCOL_VERSION {
            error!(
                "Proxy server's protocol version is too low! You are on `{PROTOCOL_VERSION}` but proxy is on `{protocol_version}`! Ask the maintainer to upgrade if they can!"
            );
            std::process::exit(1);
        } else {
            error!(
                "Proxy server's protocol version is too high! You are on `{PROTOCOL_VERSION}` but proxy is on `{protocol_version}`! Try upgrading your local `mineshare` version to the latest version and try again!"
            );
            std::process::exit(1);
        }
    }
    drop(v);
    info!("Fetched Url");
    if let Some(ref d) = args.request_domain
        && *d != domain
    {
        error!("Failed to get requested domain!");
    }
    info!("Proxy url: {domain}");
    main_loop(proxy_conn, args).await;
    Ok(())
}

async fn main_loop<S: AsyncRead + AsyncWrite + Unpin + Send>(mut conn: S, args: &Args) {
    let abort = Abort::new();
    let proxy_play: Arc<str> = Arc::from(format!(
        "{}:{}",
        args.proxy_server, args.proxy_server_play_port
    ));
    let mut buf = [0u8; 512];
    loop {
        let msg = match Message::decode(&mut conn, &mut buf).await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error parsing message send by server: {e}. Aborting.");
                abort.abort();
                return;
            }
        };
        let id = match msg {
            Message::HeartBeat(data) => {
                if let Err(e) = conn.write_all(&data).await {
                    error!("Writing heartbeat echo to server failed: {e}");
                    abort.abort();
                    return;
                }
                continue;
            }
            Message::NewClient(id) => id,
        };

        info!("Server sent request with id: {id}");
        let proxy_play = proxy_play.clone();
        let saddr = args.server_socket_addr.clone();
        let abort = abort.clone();
        tokio::task::spawn(async move {
            info!("Starting proxy PLAY request with id {id}");
            let proxy_stream = TcpStream::connect(&*proxy_play).await;
            let mut proxy_stream = match proxy_stream {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to proxy's PLAY port: {e}");
                    return;
                }
            };
            info!("Proxy PLAY connected");
            info!("Connecting to MC server stream");
            let server_stream = TcpStream::connect(saddr).await;
            let server_stream = match server_stream {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to MC server: {e}");
                    error!("Is the MC server up?");
                    return;
                }
            };
            info!("Connected to MC server");
            let server_addr = match server_stream.peer_addr() {
                Ok(a) => a,
                Err(e) => {
                    error!("Failed to fetch server's peer addr: {e}");
                    return;
                }
            };
            if let Err(e) = proxy_stream.write_u128(id).await {
                error!("Failed to send ID to server: {e}");
                return;
            }
            if let Err(e) = proxy_stream.flush().await {
                error!("Failed to send ID to server: {e}");
                return;
            }
            handle_duplex(proxy_stream, server_addr, server_stream, abort).await;
        });
    }
}

async fn handle_duplex(
    mut proxy_server_stream: TcpStream,
    mc_server_addr: SocketAddr,
    mut mc_server_stream: TcpStream,
    abort: Abort,
) {
    let mut buf = vec![0u8; 128];
    let client_addr = match Addr::decode(&mut proxy_server_stream, &mut buf).await {
        Ok(Addr(client_addr)) => client_addr,
        Err(e) => {
            error!("Failed to read socketaddr from client connection: {e}");
            _ = proxy_server_stream.shutdown().await;
            _ = mc_server_stream.shutdown().await;
            return;
        }
    };
    drop(buf);
    let mut proxy_stream = proxy_server_stream;
    info!("Connected {client_addr} to {mc_server_addr}");
    let mut buf1 = vec![0u8; 128 * 1024];
    let mut buf2 = vec![0u8; 128 * 1024];
    loop {
        select! {
            _abort = abort.wait() => {
                _ = proxy_stream.shutdown().await;
                _ = mc_server_stream.shutdown().await;
                return;
            }
            res = proxy_stream.read(&mut buf1) => {
                match res {
                    Ok(0) => {
                        info!("Client {client_addr} ended connection with {mc_server_addr}");
                        _ = mc_server_stream.shutdown().await;
                        return;
                    }
                    Ok(amt) => {
                        if let Err(e) = mc_server_stream.write_all(&buf1[..amt]).await {
                            info!("Error when writing to mc server {mc_server_addr} by client {client_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = proxy_stream.shutdown().await;
                            _ = mc_server_stream.shutdown().await;
                            return;
                        }
                        if let Err(e) = mc_server_stream.flush().await {
                            info!("Error when writing to mc server {mc_server_addr} by client {client_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = proxy_stream.shutdown().await;
                            _ = mc_server_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr} connected to {mc_server_addr}: {e}");
                        // If one of the connections errors, we should abort the other one too.
                        // We ignore the returned results because there's nothing we can do if the disconnection fails.
                        _ = proxy_stream.shutdown().await;
                        _ = mc_server_stream.shutdown().await;
                        return;
                    },
                }
            }
            res = mc_server_stream.read(&mut buf2) => {
                match res {
                    Ok(0) => {
                        info!("Client {mc_server_addr} ended connection with {client_addr}");
                        _ = mc_server_stream.shutdown().await;
                        return;
                    }
                    Ok(amt) => {
                        if let Err(e) = proxy_stream.write_all(&buf2[..amt]).await {
                            info!("Error when writing to client {client_addr} by server {mc_server_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = mc_server_stream.shutdown().await;
                            _ = proxy_stream.shutdown().await;
                            return;
                        }
                        if let Err(e) = proxy_stream.flush().await {
                            info!("Error when writing to client {client_addr} by server {mc_server_addr}: {e}");
                            // If one of the connections errors, we should abort the other one too.
                            // We ignore the returned results because there's nothing we can do if the disconnection fails.
                            _ = mc_server_stream.shutdown().await;
                            _ = proxy_stream.shutdown().await;
                            return;
                        }
                    },
                    Err(e) => {
                        info!("Error when reading from client {client_addr} connected to {mc_server_addr}: {e}");
                        // If one of the connections errors, we should abort the other one too.
                        // We ignore the returned results because there's nothing we can do if the disconnection fails.
                        _ = mc_server_stream.shutdown().await;
                        _ = proxy_stream.shutdown().await;
                        return;
                    },
                }
            }
        }
    }
}

#[derive(Parser, Debug)]
#[clap(version, about)]
struct Args {
    /// The proxy server URL. Leave blank to connect to the default proxy.
    #[arg(long, default_value = DEFAULT_URL)]
    proxy_server: String,
    /// The proxy server's PLAY port. This is the port that the host server
    /// connects to once requested by the proxy server, to start proxying the connection.
    /// Leave blank to connect to the default port
    #[arg(long, default_value_t = 25564)]
    proxy_server_play_port: u16,
    /// The proxy server's INIT port. This is the port that the host server
    /// connects to in order to request proxying
    /// Leave blank to connect to the default port
    #[arg(long, default_value_t = 25563)]
    proxy_server_init_port: u16,
    /// The Minecraft server that you want to proxy.
    server_socket_addr: String,
    /// A domain to request the proxy server to assign to us. This can be used as a sort of "static url" for your server.
    /// Leave blank to get auto-assigned a domain.
    #[arg(short, long)]
    request_domain: Option<String>,
}

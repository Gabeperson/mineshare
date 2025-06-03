#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use clap::Parser;
use mineshare::Message;
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::{io::ErrorKind, net::SocketAddr, str::FromStr as _, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tokio_rustls::TlsConnector;
use tracing::{Level, error, info};

const DEFAULT_URL: &str = "mc.mineshare.dev";

#[tokio::main]
async fn main() {
    async_main().await;
}

async fn async_main() {
    tracing_subscriber::fmt::fmt()
        .with_max_level(Level::INFO)
        .init();
    let args = Args::parse();
    info!("Starting proxy connection");
    let proxy_conn = match TcpStream::connect(&format!("{}:443", &args.proxy_server)).await {
        Ok(l) => l,
        Err(e) => {
            error!(
                "Failed to connect to proxy server `{}`: {e}",
                args.proxy_server
            );
            std::process::exit(1);
        }
    };
    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let mut rustls_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    rustls_config.alpn_protocols = vec![b"mineshare".into()];
    let rustls_config = Arc::new(rustls_config);

    let server_name: ServerName = match args.proxy_server.clone().try_into() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Invalid proxy server domain `{}`: {e}", args.proxy_server);
            std::process::exit(1);
        }
    };
    let mut proxy_conn = match TlsConnector::from(rustls_config)
        .connect(server_name, proxy_conn)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to create TLS connection with proxy server: {e}");
            std::process::exit(1);
        }
    };
    info!("Proxy connection completed");
    info!("Fetching Url");
    let mut u64_buf = [0u8; 8];
    let domain_fut = async {
        proxy_conn.read_exact(&mut u64_buf).await?;
        let len = u64::from_be_bytes(u64_buf) as usize;
        let mut domain_buf = vec![0u8; 256];
        let len = proxy_conn.read_exact(&mut domain_buf[..len]).await?;
        domain_buf.truncate(len);
        let s = match String::from_utf8(domain_buf) {
            Ok(s) => s,
            Err(e) => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, e));
            }
        };
        Ok(s)
    };
    let domain: Result<String, std::io::Error> = domain_fut.await;
    let domain = match domain {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to read domain from proxy sever: {e}");
            std::process::exit(1);
        }
    };
    info!("Fetched Url");
    info!("Proxy url: {domain}");

    _ = tokio::task::spawn(main_loop(proxy_conn, args));
    tokio::signal::ctrl_c().await.unwrap();
}

async fn main_loop<S: AsyncRead + AsyncWrite + Unpin>(mut conn: S, args: Args) {
    let proxy_play: Arc<str> = Arc::from(format!(
        "{}:{}",
        args.proxy_server, args.proxy_server_play_port
    ));
    let mut buf = [0u8; 512];
    let mut read_amt = 0;
    loop {
        match conn.read(&mut buf[read_amt..]).await {
            Ok(read) => {
                read_amt += read;
            }
            Err(e) => {
                error!("Server disconnected: {e}");
                std::process::exit(1);
            }
        }
        if read_amt < 8 {
            // Can't read length yet
            continue;
        }
        let len = u64::from_be_bytes(buf[..8].try_into().unwrap());
        if read_amt - 8 < len as usize {
            // Can't read body of msg yet
            continue;
        }
        let (msg, _len) = match bincode::decode_from_slice::<Message, _>(
            &buf[8..8 + len as usize],
            bincode::config::standard(),
        ) {
            Ok(m) => m,
            Err(e) => {
                error!("Server sent message that couldn't be decoded: {e}");
                std::process::exit(1);
            }
        };
        buf.copy_within(8 + len as usize.., 0);
        read_amt -= 8 + len as usize;
        let id = match msg {
            Message::HeartBeat(data) => {
                if let Err(e) = send_heartbeat(&mut conn, data).await {
                    error!("Writing heartbeat echo to server failed: {e}");
                    std::process::exit(1);
                }
                continue;
            }
            Message::HeartBeatEcho(_) => {
                error!("Proxy server sent ECHO?");
                std::process::exit(1);
            }
            Message::NewClient(id) => id,
        };

        info!("Server sent request with id: {id}");
        // let addr = args.proxy_server_play.clone();
        let proxy_play = proxy_play.clone();
        let saddr = args.server_socket_addr.clone();
        tokio::task::spawn(async move {
            info!("Starting proxy PLAY request with id {id}");
            let proxy_stream = TcpStream::connect(&*proxy_play).await;
            let mut proxy_stream = match proxy_stream {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to proxy's PLAY port: {e}");
                    std::process::exit(1);
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
                    std::process::exit(1);
                }
            };
            if let Err(e) = proxy_stream.write_u128(id).await {
                error!("Failed to send ID to server: {e}");
                std::process::exit(1);
            }
            if let Err(e) = proxy_stream.flush().await {
                error!("Failed to send ID to server: {e}");
                std::process::exit(1);
            }
            handle_duplex(proxy_stream, server_addr, server_stream).await;
        });
    }
}

async fn send_heartbeat<S: AsyncWrite + AsyncRead + Unpin>(
    conn: &mut S,
    data: [u8; 32],
) -> Result<(), std::io::Error> {
    let mut buf = [0u8; 64];
    let encoded_len = bincode::encode_into_slice(
        Message::HeartBeatEcho(data),
        &mut buf,
        bincode::config::standard(),
    )
    .expect("Serializing hearteat response should never fail!");
    conn.write_u64(encoded_len as u64).await?;
    conn.write_all(&buf[..encoded_len]).await?;
    Ok(())
}

async fn handle_duplex(
    mut proxy_server_stream: TcpStream,
    mc_server_addr: SocketAddr,
    mut mc_server_stream: TcpStream,
) {
    let mut buf1 = vec![0u8; 32 * 1024];

    let client_addr_fut = async {
        let len = proxy_server_stream.read_u64().await? as usize;
        let len = proxy_server_stream.read_exact(&mut buf1[..len]).await?;
        let s = match str::from_utf8(&buf1[..len]) {
            Ok(s) => s,
            Err(e) => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, e));
            }
        };
        let socketaddr = match SocketAddr::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, e));
            }
        };
        Ok::<SocketAddr, std::io::Error>(socketaddr)
    };
    let client_addr = match client_addr_fut.await {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to read socketaddr from client connection: {e}");
            _ = proxy_server_stream.shutdown().await;
            _ = mc_server_stream.shutdown().await;
            return;
        }
    };
    let mut proxy_stream = proxy_server_stream;
    drop(buf1);
    info!("Connected {client_addr} to {mc_server_addr}");
    info!("Beginning proxying...");
    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];
    loop {
        select! {
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
struct Args {
    /// The proxy server URL. Leave blank to connect to the default proxy.
    #[arg(long, default_value = DEFAULT_URL)]
    proxy_server: String,
    /// The proxy server's PLAY port. This is the port that the host server
    /// connects to once requested by the proxy server, to start proxying the connection.
    /// Leave blank to connect to the default port
    #[arg(long, default_value_t = 25564)]
    proxy_server_play_port: u16,
    /// The MC server that you want to proxy.
    server_socket_addr: String,
}

#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
use clap::Parser;
use ed25519_dalek::VerifyingKey;
use mineshare::{
    Addr, BincodeAsync as _, DomainAndPubKey, Message, PROTOCOL_VERSION, ServerHello,
    dhauth::AuthenticatorServer,
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::{net::SocketAddr, sync::Arc};
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
            error!("Invalid proxy server domain `{}`: {e}", args.proxy_server);
            std::process::exit(1);
        }
    };
    let mut proxy_conn = match TlsConnector::from(rustls_config)
        .connect(server_name, proxy_conn)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create TLS connection with proxy server: {e}");
            std::process::exit(1);
        }
    };
    info!("Proxy connection completed");
    info!("Sending initial hello to proxy server");
    let mut v = vec![0u8; 512];
    if let Err(e) = ServerHello("mineshare", args.requested_domain.as_deref())
        .encode(&mut proxy_conn, &mut v)
        .await
    {
        error!("Error sending server hello to proxy server: {e}");
    }
    info!("Sent initial hello to proxy server");
    info!("Fetching Url");
    let DomainAndPubKey {
        domain,
        public_key: alice_pubkey,
        protocol_version,
    } = match DomainAndPubKey::parse(&mut proxy_conn, &mut v).await {
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
    if let Some(ref d) = args.requested_domain {
        if *d != domain {
            error!("Failed to get requested domain!");
        }
    }
    info!("Proxy url: {domain}");

    _ = tokio::task::spawn(main_loop(proxy_conn, args, alice_pubkey));
    tokio::signal::ctrl_c().await.unwrap();
}

async fn main_loop<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut conn: S,
    args: Args,
    alice_pubkey: [u8; 32],
) {
    let pubkey = VerifyingKey::from_bytes(&alice_pubkey).expect("Server send invalid public key?");
    let proxy_play: Arc<str> = Arc::from(format!(
        "{}:{}",
        args.proxy_server, args.proxy_server_play_port
    ));
    let mut buf = [0u8; 512];
    loop {
        let msg = match Message::parse(&mut conn, &mut buf).await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error parsing message send by server: {e}");
                std::process::exit(1);
            }
        };
        let id = match msg {
            Message::HeartBeat(data) => {
                let mut buf = [0u8; 128];
                if let Err(e) = Message::HeartBeatEcho(data)
                    .encode(&mut conn, &mut buf)
                    .await
                {
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
            let auth = AuthenticatorServer {
                inner: &mut proxy_stream,
                alice_public_sign_key: pubkey,
            };
            if let Err(e) = auth.send_id(id).await {
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

async fn handle_duplex(
    mut proxy_server_stream: TcpStream,
    mc_server_addr: SocketAddr,
    mut mc_server_stream: TcpStream,
) {
    let mut buf = vec![0u8; 128];
    let client_addr = match Addr::parse(&mut proxy_server_stream, &mut buf).await {
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
    /// The MC server that you want to proxy.
    server_socket_addr: String,
    /// A domain to request the proxy server to assign to us. This can be used as a sort of "static url" for your server.
    /// Leave blank to get auto-assigned a domain.
    #[arg(long)]
    requested_domain: Option<String>,
}

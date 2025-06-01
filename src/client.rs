use clap::Parser;
use std::{io::ErrorKind, net::SocketAddr, str::FromStr as _, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tracing::{error, info};

const DEFAULT_URL: &str = "mc.gshwang.com:25564";
const DEFAULT_PLAY_URL: &str = "mc.gshwang.com:25563";

#[tokio::main]
async fn main() {
    async_main().await
}

async fn async_main() {
    let args = Args::parse();
    println!("Starting proxy connection");
    let mut proxy_conn = match TcpStream::connect(&args.proxy_server).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "Failed to connect to proxy server `{}`: {e}",
                args.proxy_server
            );
            std::process::exit(1);
        }
    };
    println!("Proxy connection completed");
    println!("Fetching url");
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
            eprintln!("Failed to read domain from proxy sever: {e}");
            std::process::exit(1);
        }
    };
    println!("Fetched Url");
    println!("Proxy url: {domain}");
    let (mut recv, mut send) = proxy_conn.into_split();
    tokio::task::spawn(async move {
        loop {
            if let Err(e) = send.write_all(b"heartbeat").await {
                eprintln!("Failed to heartbeat server: {e}");
                std::process::exit(1);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    tokio::task::spawn(async move {
        loop {
            let id = match recv.read_u128().await {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("Server disconnected: {e}");
                    std::process::exit(1);
                }
            };
            println!("Server sent request with id: {id}");
            let addr = args.proxy_server_play.clone();
            let saddr = args.server_socket_addr.clone();
            tokio::task::spawn(async move {
                println!("Starting proxy PLAY request with id {id}");
                let proxy_streanm = TcpStream::connect(addr).await;
                let mut proxy_stream = match proxy_streanm {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Failed to connect to proxy's PLAY port: {e}");
                        std::process::exit(1);
                    }
                };
                println!("Proxy PLAY connected");
                println!("Connecting to MC server stream");
                let server_stream = TcpStream::connect(saddr).await;
                let server_stream = match server_stream {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Failed to connect to server: {e}");
                        std::process::exit(1);
                    }
                };
                println!("Connected to MC server");
                let server_addr = match server_stream.peer_addr() {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("Failed to fetch server's peer addr: {e}");
                        std::process::exit(1);
                    }
                };
                if let Err(e) = proxy_stream.write_u128(id).await {
                    eprintln!("Failed to send ID to server: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = proxy_stream.flush().await {
                    eprintln!("Failed to send ID to server: {e}");
                    std::process::exit(1);
                }
                handle_duplex(proxy_stream, server_addr, server_stream).await;
            });
        }
    });
    tokio::signal::ctrl_c().await.unwrap();
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
    println!("Connected {client_addr} to {mc_server_addr}");
    println!("Beginning proxying...");
    let mut buf1 = vec![0u8; 32 * 1024];
    let mut buf2 = vec![0u8; 32 * 1024];
    loop {
        select! {
            res = proxy_stream.read(&mut buf1) => {
                match res {
                    Ok(0) => {
                        println!("Client {client_addr} ended connection with {mc_server_addr}");
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
                        println!("Client {mc_server_addr} ended connection with {client_addr}");
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
    #[arg(long, default_value = DEFAULT_URL)]
    proxy_server: String,
    #[arg(long, default_value = DEFAULT_PLAY_URL)]
    proxy_server_play: String,
    server_socket_addr: String,
}

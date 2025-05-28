use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::sync::mpsc::{Receiver, Sender, channel};
#[tokio::main]
async fn main() {
    async_main().await
}

async fn async_main() {
    let args = Args::parse();
    let map = Arc::new(RwLock::new(
        HashMap::<Vec<u8>, Sender<(TcpStream, Vec<u8>)>>::new(),
    ));
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
}

#[derive(Parser, Debug)]
struct Args {
    base_domain: String,
    #[arg(default_value = "localhost:25565")]
    client_socket_addr: String,
    #[arg(default_value = "localhost:25564")]
    server_socket_addr: String,
}

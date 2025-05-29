use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, pin_mut};
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use mineshare::*;
use rand::Rng as _;
use rand::seq::IndexedRandom;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncRead as _, AsyncWriteExt as _, ReadBuf};
use tokio::time::Instant;

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
    let base_domain: Arc<str> = Arc::from(args.base_domain);
    let map = Arc::new(RwLock::new(HashMap::<Vec<u8>, Sender<TcpStream>>::new()));
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
            tokio::task::spawn(async move {
                let res = server_handler(stream, addr, map, base_domain).await;
                if let Err(e) = res {
                    eprintln!("Server handler returned error: {e}");
                }
            });
        }
    });
}

async fn server_handler(
    mut stream: TcpStream,
    addr: SocketAddr,
    map: Arc<RwLock<HashMap<Vec<u8>, Sender<TcpStream>>>>,
    base_domain: Arc<str>,
) -> Result<()> {
    let mut prefix = {
        let locked = map.read().await;
        get_random_prefix(&locked)
    };
    prefix.push(b'.');
    prefix.extend(base_domain.as_bytes());
    let url = prefix;
    Message::DomainDecided(&url).encode(&mut stream).await?;

    let mut clients: Vec<SingleRecvConn> = Vec::new();
    let mut recv_buffer_limits: Vec<RecvMux> = Vec::new();
    let mut timeout_time = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
    let mut index_client = 0;
    let mut index_multi = 0;

    loop {
        let client_fut = ArrayPollSingle {
            arr: Some(&mut clients),
            index: Some(&mut index_client),
        };
        let recv_fut = ArrayPollMux {
            arr: Some(&mut recv_buffer_limits),
            index: Some(&mut index_multi),
        };
        let heartbeat = tokio::time::sleep(Duration::from_secs_f32(TIMEOUT_DURATION_SECS));
    }
}

// struct ReadClientVec<'a> {
//     inner: &'a mut [Client],
//     buf: ReadBuf<'a>,
// }

// impl<'a> Future for ReadClientVec<'a> {
//     type Output = ();

//     fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
//         for client in self.inner {
//             let mut pin = Pin::new(&mut client.stream);
//             match pin.poll_read(&mut self.buf, cx) {}
//         }

//         todo!()
//     }
// }

fn get_random_prefix(map: &HashMap<Vec<u8>, Sender<TcpStream>>) -> Vec<u8> {
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
    #[arg(long, default_value = "localhost:25565")]
    client_socket_addr: String,
    #[arg(long, default_value = "localhost:25564")]
    server_socket_addr: String,
}

use anyhow::Result;
use mineshare::*;
use rand::seq::IndexedRandom;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt;
use tokio::select;
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
                    eprintln!("Server exited with error: {e}");
                }
            });
        }
    });

    tokio::signal::ctrl_c().await.unwrap();
}

async fn server_handler(
    mut stream: TcpStream,
    addr: SocketAddr,
    map: Arc<RwLock<HashMap<Vec<u8>, Sender<TcpStream>>>>,
    base_domain: Arc<str>,
) -> Result<()> {
    println!("{addr} connected");
    let mut prefix = {
        let locked = map.read().await;
        get_random_prefix(&locked)
    };
    prefix.push(b'.');
    prefix.extend(base_domain.as_bytes());
    let url = prefix;
    Message::DomainDecided(&url).encode(&mut stream).await?;

    let mut map: HashMap<u64, usize> = HashMap::new();
    let mut clients: Vec<SingleRecvConn> = Vec::new();
    let mut recv_buffer_limits: Vec<RecvMux> = Vec::new();
    let mut timeout_time = Instant::now() + Duration::from_secs_f32(TIMEOUT_DURATION_SECS);
    let mut index_client = 0;
    let mut index_multi = 0;
    let mut recv_buf = Box::new([0u8; MUX_RECV_BUF_SIZE]);
    let mut recv_buf_idx = 0;

    // TODO channel to send the things here
    // TODO timeout

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
        let read_server = stream.read(&mut recv_buf[recv_buf_idx..]);
        select!(
            (res, index) = client_fut => {
                match res {
                    Ok(len) => {
                        if len == 0 {
                            // Connection reached EOF. So we close.
                            Message::Disconnect(index as u64).encode(&mut stream).await?;
                            clients.swap_remove(index);
                            recv_buffer_limits.swap_remove(index);
                            map = clients.iter().enumerate().map(|(index, client)| {
                                (client.id, index)
                            }).collect();
                        }
                        stream.write_all(&clients[index].buf[..len]).await?;
                    },
                    Err(_e) => {
                        // This connection errored, tell the other side and remove them
                        Message::Disconnect(index as u64).encode(&mut stream).await?;
                        clients.swap_remove(index);
                        recv_buffer_limits.swap_remove(index);
                        map = clients.iter().enumerate().map(|(index, client)| {
                            (client.id, index)
                        }).collect();
                    },
                }
            }
            (send_amt, index) = recv_fut => {
                let client = &mut clients[index];
                let recvmux = &mut recv_buffer_limits[index];
                let buf = &recvmux.buf.inner[..send_amt];
                match client.stream.write(buf).await {
                    Ok(written) => {
                        recvmux.buf.wrote(written);
                        Message::Acknowledged(send_amt as u64).encode(&mut stream).await?;
                    },
                    Err(_e) => {
                        // This connection errored, tell the other side and remove them
                        Message::Disconnect(index as u64).encode(&mut stream).await?;
                        clients.swap_remove(index);
                        recv_buffer_limits.swap_remove(index);
                        map = clients.iter().enumerate().map(|(index, client)| {
                            (client.id, index)
                        }).collect();
                    },
                }
            }
            _ = heartbeat => {
                Message::HeartBeat.encode(&mut stream).await?
            }
            read_amt = read_server => {
                let read_amt = read_amt?;
                match Message::decode(&recv_buf[..recv_buf_idx+read_amt]) {
                    Ok(msg) => {
                        let read: usize;
                        match msg {
                            (Message::DomainDecided(_) | Message::Connect(_) | Message::Acknowledged(_), _read) => {
                                anyhow::bail!("Invalid messages for client server to send.")
                            }
                            (Message::Disconnect(id), read_amt) => {
                                let index = *map.get(&id).unwrap();
                                clients.swap_remove(index);
                                recv_buffer_limits.swap_remove(index);
                                map = clients.iter().enumerate().map(|(index, client)| {
                                    (client.id, index)
                                }).collect();
                                read = read_amt;
                            },
                            (Message::Data(id, data), read_amt) => {
                                let index = *map.get(&id).unwrap();
                                let client_buf = &mut recv_buffer_limits[index];
                                if client_buf.buf.available() < data.len() {
                                    anyhow::bail!("Server sent too much data")
                                }
                                client_buf.buf.write(data);
                                read = read_amt;
                            }
                            (Message::HeartBeat, read_amt) => {
                                read = read_amt;
                            }
                        }
                        recv_buf.copy_within(read.., 0);
                        recv_buf_idx = 0;
                    },
                    Err(DecodeStatus::NeedMoreData) => {
                        // Need more data, so we just update the cursor and continue.
                        recv_buf_idx += read_amt;
                    },
                    Err(DecodeStatus::Error(e)) => {
                        return Err(e.into());
                    }
                }
            }
        )
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

use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio::io::AsyncWriteExt;

use iroh::{Endpoint, NodeId, endpoint::Connecting};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
};

pub async fn server(
    server_ip: SocketAddr,
    conn_alpn: &str,
    ping_alpn: &str,
    ping_msg: &str,
    pong_msg: &str,
) {
    let endpoint = match iroh::Endpoint::builder()
        .alpns(vec![conn_alpn.into(), ping_alpn.into()])
        .discovery_n0()
        .bind()
        .await
    {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("Error occured when starting iroh server listener: {e}");
            return;
        }
    };
    let ping_msg: Arc<str> = Arc::from(ping_msg);
    let pong_msg: Arc<str> = Arc::from(pong_msg);
    let ping_alpn: Arc<[u8]> = Arc::from(ping_alpn.as_bytes());
    println!("Successfully set up iroh listening server.");
    println!("Server Node ID: {}", endpoint.node_id());
    println!("Ready for connections");
    loop {
        let incoming = endpoint.accept().await.expect("Endpoint wasn't closed");
        let ping_alpn = ping_alpn.clone();
        let ping_msg = ping_msg.clone();
        let pong_msg = pong_msg.clone();
        tokio::task::spawn(async move {
            let remote = incoming.remote_address();
            println!("Incoming connection from {}.", remote,);
            let connecting = match incoming.accept() {
                Ok(mut connecting) => {
                    let alpn = match connecting.alpn().await {
                        Ok(alpn) => alpn,
                        Err(e) => {
                            eprintln!("Error getting ALPN: {e}");
                            return;
                        }
                    };
                    if alpn == *ping_alpn {
                        handle_ping(connecting, remote, &ping_msg, &pong_msg).await;
                        return;
                    }
                    connecting
                }
                Err(e) => {
                    eprintln!(
                        "Error when establishing incoming iroh connection with client {remote}: {e}"
                    );
                    return;
                }
            };
            let connection = match connecting.await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!(
                        "Error when establishing incoming iroh connection with client {remote}: {e}"
                    );
                    return;
                }
            };
            let (mut remote_send, mut remote_recv) = match connection.accept_bi().await {
                Ok(streams) => streams,
                Err(e) => {
                    eprintln!(
                        "Error when creating bidirectional iroh channel with client {remote}: {e}"
                    );
                    return;
                }
            };
            let mut server_conn = match TcpStream::connect(server_ip).await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!(
                        "Error when creating connection to the host server at {server_ip}: {e}"
                    );
                    return;
                }
            };
            let (mut server_recv, mut server_send) = server_conn.split();
            println!("Successfully began proxying {remote} to server");
            select! {
                res = tokio::io::copy(&mut remote_recv, &mut server_send) => {
                    match res {
                        Ok(_) => {
                            if let Err(e) = server_send.shutdown().await {
                                eprintln!("Sending shutdown signal to client failed: {e}");
                                return;
                            };
                            println!("Client {remote} disconnected successfully");
                        },
                        Err(e) => {
                            eprintln!("Client {remote} disconnected with error: {e}");
                        },
                    }
                },
                res = tokio::io::copy(&mut server_recv, &mut remote_send) => {
                    match res {
                        Ok(_) => {
                            remote_send.finish().unwrap();
                            if let Err(e) = remote_send.stopped().await {
                                eprintln!("Waiting for client to receive stop signal failed: {e}");
                            }
                            println!("Client {remote} disconnected successfully")
                        },
                        Err(e) => {
                            eprintln!("Client {remote} disconnected with error: {e}");
                        },
                    }
                },
            };
        });
    }
}

pub async fn client(
    listen_ip: SocketAddr,
    node_id: &str,
    conn_alpn: &str,
    ping_alpn: &str,
    ping_msg: &str,
    pong_msg: &str,
) {
    let conn_alpn: Arc<[u8]> = Arc::from(conn_alpn.as_bytes());
    let server_nodeid = match NodeId::from_str(node_id) {
        Ok(nodeid) => nodeid,
        Err(e) => {
            eprintln!("Error when parsing key: {e}");
            return;
        }
    };
    let listener = match TcpListener::bind(listen_ip).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("Failed to start local tcplistener: {e}");
            return;
        }
    };
    let localaddr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("Failed to get local server ip from local_addr: {e}");
            return;
        }
    };
    let endpoint = match Endpoint::builder().discovery_n0().bind().await {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("Failed to create client iroh endpoint: {e}");
            return;
        }
    };
    {
        println!("Pinging server");
        let conn = match endpoint.connect(server_nodeid, ping_alpn.as_ref()).await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Error establishing PING connection to server: {e}");
                return;
            }
        };
        let (mut send, mut recv) = match conn.open_bi().await {
            Ok(streams) => streams,
            Err(e) => {
                eprintln!("Error establishing bidirectional PING streams with server: {e}");
                return;
            }
        };
        if let Err(e) = send.write_all(ping_msg.as_bytes()).await {
            eprintln!("Error sending PING message to server: {e}");
            return;
        }
        send.finish().unwrap();
        let data = match recv.read_to_end(pong_msg.len()).await {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Error receiving PONG message from server: {e}");
                return;
            }
        };
        if data != pong_msg.as_bytes() {
            eprintln!("Invalid PONG message received from client");
            return;
        }
        println!("Pinging server successfuly completed");
    }
    let endpoint = Arc::new(endpoint);
    println!("Listening for connections at {}", localaddr);
    loop {
        let fut = listener.accept().await;
        let endpoint = endpoint.clone();
        let conn_alpn = conn_alpn.clone();
        tokio::task::spawn(async move {
            let (mut client, clientaddr) = match fut {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Error accepting TCP connection from client: {e}");
                    return;
                }
            };
            println!("Client {clientaddr} connected to local server");
            let (mut client_recv, mut client_send) = client.split();
            let server_conn = match endpoint.connect(server_nodeid, conn_alpn.as_ref()).await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Error when establishing connection with server: {e}");
                    return;
                }
            };
            let (mut server_send, mut server_recv) = match server_conn.open_bi().await {
                Ok(streams) => streams,
                Err(e) => {
                    eprintln!(
                        "Error when establishing bidirectional iroh channel with server: {e}"
                    );
                    return;
                }
            };
            println!("Successfully began proxying {clientaddr} to server");
            select! {
                res = tokio::io::copy(&mut client_recv, &mut server_send) => {
                    match res {
                        Ok(_) => {
                            server_send.finish().unwrap();
                            if let Err(e) = server_send.stopped().await {
                                eprintln!("Waiting for server to receive stop signal failed: {e}");
                            }
                            println!("Client {clientaddr} disconnected successfully")
                        },
                        Err(e) => {
                            eprintln!("Client {clientaddr} disconnected with error: {e}");
                        },
                    }
                },
                res = tokio::io::copy(&mut server_recv, &mut client_send) => {
                    match res {
                        Ok(_) => {
                            if let Err(e) = server_send.shutdown().await {
                                eprintln!("Sending shutdown signal to server failed: {e}");
                                return;
                            };
                            println!("Client {clientaddr} disconnected successfully")
                        },
                        Err(e) => {
                            eprintln!("Client {clientaddr} disconnected with error: {e}");
                        },
                    }
                },
            };
        });
    }
}

async fn handle_ping(c: Connecting, remote: SocketAddr, ping_msg: &str, pong_msg: &str) {
    let conn = match c.await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Error when receiving connection for PING request: {e}");
            return;
        }
    };
    println!("Received PING request from {remote}");
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            eprintln!("Error establishing bidirectional PING connection with client: {e}");
            return;
        }
    };

    let data = match recv.read_to_end(ping_msg.len()).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Error receiving PING message from client: {e}");
            return;
        }
    };
    if data != ping_msg.as_bytes() {
        eprintln!("Invalid PING message received from client");
        return;
    }
    if let Err(e) = send.write_all(pong_msg.as_bytes()).await {
        eprintln!("Error when sending PONG response back to client: {e}");
        return;
    }
    send.finish().unwrap();
    if let Err(e) = send.stopped().await {
        eprintln!("Error when waiting for client to finish reading PONG: {e}");
    };
    println!("Ping pong with {remote} successfully completed");
}

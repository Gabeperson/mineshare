use crate::Addr;
use crate::InitResponse;
use crate::Message;
use crate::PROTOCOL_VERSION;
use crate::ServerHello;
use crate::StreamHelper as _;

use super::types::*;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tokio_util::sync::CancellationToken;

pub async fn server_thread(mut recv: flume::Receiver<UiEvent>, send: flume::Sender<ServerEvent>) {
    loop {
        let Ok(event) = recv.recv_async().await else {
            // If None, it means main thread has quit.
            return;
        };
        match event {
            // We're already stopped, so noop
            UiEvent::Stop => {}
            // The server isn't even running, so noop
            UiEvent::Disconnect(_socket_addr) => {}
            UiEvent::Start(connect_options) => {
                connect_try(&connect_options, &mut recv, &send).await;
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn connect_try(
    options: &ConnectOptions,
    recv: &mut flume::Receiver<UiEvent>,
    send: &flume::Sender<ServerEvent>,
) {
    let mut proxy_conn = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&format!(
            "{}:{}",
            options.proxy_server, options.proxy_server_init_port
        )),
    )
    .await
    {
        Ok(Ok(l)) => l,
        Err(_) => {
            _ = send
                .send_async(ServerEvent::ServerConnectionFailedTimeout)
                .await;
            return;
        }
        Ok(Err(e)) => {
            _ = send
                .send_async(ServerEvent::ServerConnectionFailed(format!(
                    "Failed to connect to server: {e}"
                )))
                .await;
            return;
        }
    };
    let mut v = vec![0u8; 512];
    if let Err(e) = ServerHello::new(options.request_domain.as_deref())
        .encode(&mut proxy_conn, &mut v)
        .await
    {
        _ = send
            .send_async(ServerEvent::ServerConnectionFailed(format!(
                "Failed to send Hello: {e}",
            )))
            .await;
        return;
    }
    let InitResponse {
        domain,
        protocol_version,
    } = match InitResponse::decode(&mut proxy_conn, &mut v).await {
        Ok(d) => d,
        Err(e) => {
            _ = send
                .send_async(ServerEvent::ServerConnectionFailed(format!(
                    "Failed to fetch domain from proxy server: {e}"
                )))
                .await;
            return;
        }
    };
    if protocol_version != PROTOCOL_VERSION {
        _ = send
            .send_async(ServerEvent::InvalidProtocolVersion(protocol_version))
            .await;
        return;
    }
    drop(v);
    if let Some(ref d) = options.request_domain
        && *d != domain
    {
        _ = send
            .send_async(ServerEvent::DidntGetRequestedUrl(domain))
            .await;
        return;
    }
    let cancel = CancellationToken::new();
    _ = send.send_async(ServerEvent::Url(domain)).await;
    let (send_client, recv_client) = flume::bounded::<ClientStatusUpdate>(10);
    let handle_events = async {
        let mut map = HashMap::new();
        loop {
            tokio::select! {
                command = recv_client.recv_async() => {
                    let Ok(command) = command else {
                        return;
                    };
                    match command {
                        ClientStatusUpdate::Connected { addr, token } => {
                            map.insert(addr, token);
                        },
                        ClientStatusUpdate::Disconnected { addr } => {
                            map.remove(&addr);
                        },
                    }
                }
                event = recv.recv_async() => {
                    let Ok(event) = event else {
                        return;
                    };
                    match event {
                        UiEvent::Stop => {
                            // Return from this future which will finish the outer select! and stop everything
                            return;
                        },
                        // We're already started so this doesn't do anything
                        UiEvent::Start(_connect_options) => {},
                        UiEvent::Disconnect(socket_addr) => {
                            if let Some(token) = map.get(&socket_addr) {
                                token.cancel();
                            }
                            map.remove(&socket_addr);
                        },
                    }
                }
            }
        }
    };
    tokio::select! {
        () = main_loop(proxy_conn, options, send_client, send, cancel.clone()) => {}
        () = handle_events => {}
    }
    cancel.cancel();
    _ = send.send_async(ServerEvent::Stopped).await;
}

async fn main_loop<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut conn: S,
    options: &ConnectOptions,
    send_client: flume::Sender<ClientStatusUpdate>,
    send: &flume::Sender<ServerEvent>,
    cancel: CancellationToken,
) {
    let proxy_play: Arc<str> = Arc::from(format!(
        "{}:{}",
        options.proxy_server, options.proxy_server_play_port
    ));
    let mut buf = [0u8; 512];
    loop {
        let msg = match Message::decode(&mut conn, &mut buf).await {
            Ok(msg) => msg,
            Err(e) => {
                _ = send
                    .send_async(ServerEvent::ServerConnectionFailed(format!(
                        "Error parsing message send by server: {e}",
                    )))
                    .await;
                cancel.cancel();
                return;
            }
        };
        let id = match msg {
            Message::HeartBeat(data) => {
                if let Err(e) = conn.write_all(&data).await {
                    _ = send
                        .send_async(ServerEvent::ServerConnectionFailed(format!(
                            "Sending heartbeat to server failed: {e}"
                        )))
                        .await;
                    cancel.cancel();
                    return;
                }
                continue;
            }
            Message::NewClient(id) => id,
        };
        let proxy_play = proxy_play.clone();
        let saddr = options.server_ip.clone();
        let cancel = cancel.child_token();
        let send = send.clone();
        let send_client = send_client.clone();
        tokio::task::spawn(async move {
            let proxy_stream = TcpStream::connect(&*proxy_play).await;
            let mut proxy_stream = match proxy_stream {
                Ok(s) => s,
                Err(e) => {
                    _ = send
                        .send_async(ServerEvent::ServerNonCatastrophicError(format!(
                            "Failed to connect to proxy's PLAY port: {e}"
                        )))
                        .await;
                    return;
                }
            };
            let server_stream = match TcpStream::connect(saddr).await {
                Ok(s) => s,
                Err(e) => {
                    _ = send
                        .send_async(ServerEvent::MCServerConnectionFailed(e.to_string()))
                        .await;
                    return;
                }
            };
            let fut = async {
                proxy_stream.write_u128(id).await?;
                proxy_stream.flush().await?;
                Ok::<(), std::io::Error>(())
            };
            if let Err(e) = fut.await {
                _ = send
                    .send_async(ServerEvent::ServerNonCatastrophicError(format!(
                        "Failed to send id to server: {e}"
                    )))
                    .await;
                return;
            }
            handle_duplex(
                proxy_stream,
                server_stream,
                send.clone(),
                cancel,
                send_client,
            )
            .await;
        });
    }
}

async fn handle_duplex(
    mut proxy_stream: TcpStream,
    mut mc_server_stream: TcpStream,
    send: flume::Sender<ServerEvent>,
    cancel: CancellationToken,
    send_client: flume::Sender<ClientStatusUpdate>,
) {
    let mut buf = vec![0u8; 128];
    let client_addr = match Addr::decode(&mut proxy_stream, &mut buf).await {
        Ok(Addr(client_addr)) => client_addr,
        Err(e) => {
            _ = send
                .send_async(ServerEvent::ServerNonCatastrophicError(format!(
                    "Error fetching player IP: {e}"
                )))
                .await;
            _ = proxy_stream.shutdown().await;
            _ = mc_server_stream.shutdown().await;
            return;
        }
    };
    drop(buf);
    _ = send_client
        .send_async(ClientStatusUpdate::Connected {
            addr: client_addr,
            token: cancel.clone(),
        })
        .await;
    _ = send
        .send_async(ServerEvent::PlayerConnected(client_addr))
        .await;
    select! {
        _cancel = cancel.cancelled() => {
            _ = proxy_stream.shutdown().await;
            _ = mc_server_stream.shutdown().await;
        }
        _res = tokio::io::copy_bidirectional_with_sizes(&mut proxy_stream, &mut mc_server_stream, 32*1024, 32*1024) => {}
    }
    _ = send
        .send_async(ServerEvent::PlayerDisconnected(client_addr))
        .await;
    _ = send_client.send(ClientStatusUpdate::Disconnected { addr: client_addr });
}

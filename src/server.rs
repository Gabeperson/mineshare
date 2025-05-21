use std::{net::SocketAddr, sync::Arc};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Sender;

use iroh::{
    NodeId,
    endpoint::{Connecting, Incoming},
};
use tokio::{net::TcpStream, select};

pub enum ServerError {
    ConnectionError(String),
    MainServerConnectionError(String),
    ServerListenError(String),
    AlpnError(String),
    Ping(String),
    ChannelClosed,
    UnexpectedDisconnect(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::ConnectionError(e) => write!(f, "{e}"),
            ServerError::MainServerConnectionError(e) => write!(f, "{e}"),
            ServerError::ServerListenError(e) => write!(f, "{e}"),
            ServerError::AlpnError(e) => write!(f, "{e}"),
            ServerError::Ping(e) => write!(f, "{e}"),
            ServerError::ChannelClosed => write!(f, "Event channel closed"),
            ServerError::UnexpectedDisconnect(e) => write!(f, "{e}"),
        }
    }
}

impl From<tokio::sync::mpsc::error::SendError<ServerEvent>> for ServerError {
    fn from(_value: tokio::sync::mpsc::error::SendError<ServerEvent>) -> Self {
        Self::ChannelClosed
    }
}

pub enum ServerEvent {
    Connected(SocketAddr),
    Listening(NodeId),
    Disconnected(SocketAddr),
    PingRequestReceived(SocketAddr),
    PingPongSuccessful(SocketAddr),
    ConnectionRequest(SocketAddr),
}

impl std::fmt::Display for ServerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerEvent::Connected(socket_addr) => {
                write!(f, "Client {socket_addr} connected")
            }
            ServerEvent::Listening(public_key) => {
                write!(f, "Ready for connctions at {public_key}")
            }
            ServerEvent::Disconnected(socket_addr) => {
                write!(f, "Client {socket_addr} disconnected")
            }
            ServerEvent::PingRequestReceived(socket_addr) => {
                write!(f, "PING request from {socket_addr} received")
            }
            ServerEvent::PingPongSuccessful(socket_addr) => {
                write!(f, "PING PONG successfully completed with {socket_addr}")
            }
            ServerEvent::ConnectionRequest(socket_addr) => {
                write!(f, "Connection request received from {socket_addr}")
            }
        }
    }
}

struct ServerHandlerArgs {
    ping_alpn: Arc<[u8]>,
    ping_msg: Arc<str>,
    pong_msg: Arc<str>,
    server_ip: SocketAddr,
    incoming: Incoming,
    events: Sender<ServerEvent>,
}

async fn server_handler(data: ServerHandlerArgs) -> Result<(), ServerError> {
    let ServerHandlerArgs {
        ping_alpn,
        ping_msg,
        pong_msg,
        incoming,
        events,
        server_ip,
    } = data;
    let remote = incoming.remote_address();
    events.send(ServerEvent::ConnectionRequest(remote)).await?;
    let mut connecting = incoming.accept().map_err(|e| {
        ServerError::ConnectionError(format!(
            "Error when accepting connection from {remote}: {e}"
        ))
    })?;
    let alpn = connecting
        .alpn()
        .await
        .map_err(|e| ServerError::AlpnError(format!("Error when getting ALPN: {e}")))?;
    if alpn == *ping_alpn {
        handle_server_ping(connecting, remote, &ping_msg, &pong_msg, &events).await?;
        return Ok(());
    }
    let connection = connecting.await.map_err(|e| {
        ServerError::ConnectionError(format!(
            "Error when establishing incoming iroh connection with client {remote}: {e}"
        ))
    })?;
    let (mut remote_send, mut remote_recv) = connection.accept_bi().await.map_err(|e| {
        ServerError::ConnectionError(format!(
            "Error when creating bidirectional iroh channel with client {remote}: {e}"
        ))
    })?;
    let mut server_conn = TcpStream::connect(server_ip).await.map_err(|e| {
        ServerError::MainServerConnectionError(format!(
            "Error when creating connection to the main server at {server_ip}: {e}"
        ))
    })?;
    let (mut server_recv, mut server_send) = server_conn.split();
    events.send(ServerEvent::Connected(remote)).await?;
    select! {
        res = tokio::io::copy(&mut remote_recv, &mut server_send) => {
            match res {
                Ok(_) => {
                    if let Err(e) = server_send.shutdown().await {
                        return Err(ServerError::UnexpectedDisconnect(format!("Sending shutdown signal to client failed: {e}")))
                    };
                    events.send(ServerEvent::Disconnected(remote)).await?;
                },
                Err(e) => {
                    events.send(ServerEvent::Disconnected(remote)).await?;
                    return Err(ServerError::UnexpectedDisconnect(format!("Client {remote} disconnected with error: {e}")));
                },
            }
        },
        res = tokio::io::copy(&mut server_recv, &mut remote_send) => {
            match res {
                Ok(_) => {
                    remote_send.finish().unwrap();
                    if let Err(e) = remote_send.stopped().await {
                        return Err(ServerError::UnexpectedDisconnect(format!("Sending shutdown signal to client failed: {e}")))
                    }
                    events.send(ServerEvent::Disconnected(remote)).await?;
                },
                Err(e) => {
                    events.send(ServerEvent::Disconnected(remote)).await?;
                    return Err(ServerError::UnexpectedDisconnect(format!("Client {remote} disconnected with error: {e}")));
                },
            }
        },
    };
    Ok(())
}

pub async fn server(
    server_ip: SocketAddr,
    conn_alpn: &str,
    ping_alpn: &str,
    ping_msg: &str,
    pong_msg: &str,
    events: Sender<ServerEvent>,
    errors: Sender<ServerError>,
) {
    let endpoint = match iroh::Endpoint::builder()
        .alpns(vec![conn_alpn.into(), ping_alpn.into()])
        .discovery_n0()
        .bind()
        .await
    {
        Ok(ep) => ep,
        Err(e) => {
            errors
                .send(ServerError::ServerListenError(format!(
                    "Error occured when starting iroh server listener: {e}"
                )))
                .await
                .unwrap();
            return;
        }
    };
    let ping_msg: Arc<str> = Arc::from(ping_msg);
    let pong_msg: Arc<str> = Arc::from(pong_msg);
    let ping_alpn: Arc<[u8]> = Arc::from(ping_alpn.as_bytes());

    events
        .send(ServerEvent::Listening(endpoint.node_id()))
        .await
        .unwrap();

    loop {
        let incoming = endpoint.accept().await.expect("Endpoint wasn't closed");
        let ping_alpn = ping_alpn.clone();
        let ping_msg = ping_msg.clone();
        let pong_msg = pong_msg.clone();
        let events = events.clone();
        let errors = errors.clone();
        tokio::task::spawn(async move {
            let args = ServerHandlerArgs {
                ping_alpn,
                ping_msg,
                pong_msg,
                server_ip,
                incoming,
                events,
            };
            match server_handler(args).await {
                Ok(()) => (),
                Err(e) => {
                    errors.send(e).await.unwrap();
                }
            }
        });
    }
}

async fn handle_server_ping(
    c: Connecting,
    remote: SocketAddr,
    ping_msg: &str,
    pong_msg: &str,
    events: &Sender<ServerEvent>,
) -> Result<(), ServerError> {
    let conn = c.await.map_err(|e| {
        ServerError::Ping(format!(
            "Error when receiving connection for PING request from {remote}: {e}",
        ))
    })?;
    events
        .send(ServerEvent::PingRequestReceived(remote))
        .await?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| {
        ServerError::Ping(format!(
            "Error establishing bidirectional PING connection with client {remote}: {e}"
        ))
    })?;
    let data = recv.read_to_end(ping_msg.len()).await.map_err(|e| {
        ServerError::Ping(format!(
            "Error receiving PING message from client {remote}: {e}"
        ))
    })?;

    if data != ping_msg.as_bytes() {
        return Err(ServerError::Ping(format!(
            "Invalid PING message received from client {remote}, expected `{ping_msg}` received {}",
            String::from_utf8_lossy(&data)
        )));
    }
    if let Err(e) = send.write_all(pong_msg.as_bytes()).await {
        return Err(ServerError::Ping(format!(
            "Error when sending PONG response back to client {remote}: {e}"
        )));
    }
    // Should never fail according to docs
    send.finish().unwrap();
    if let Err(e) = send.stopped().await {
        return Err(ServerError::Ping(format!(
            "Error when waiting for client {remote} to finish reading PONG: {e}"
        )));
    };
    events.send(ServerEvent::PingPongSuccessful(remote)).await?;
    Ok(())
}

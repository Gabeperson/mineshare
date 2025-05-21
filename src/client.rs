use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Sender;

use iroh::{Endpoint, NodeId};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
};

pub enum ClientError {
    ConnectionError(String),
    ListenError(String),
    EndpointSetup(String),
    AlpnError(String),
    Ping(String),
    ChannelClosed,
    UnexpectedDisconnect(String),
    InvalidNodeId(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::ConnectionError(e) => write!(f, "{e}"),
            ClientError::ListenError(e) => write!(f, "{e}"),
            ClientError::EndpointSetup(e) => write!(f, "{e}"),
            ClientError::AlpnError(e) => write!(f, "{e}"),
            ClientError::Ping(e) => write!(f, "{e}"),
            ClientError::ChannelClosed => write!(f, "Event channel closed"),
            ClientError::UnexpectedDisconnect(e) => write!(f, "{e}"),
            ClientError::InvalidNodeId(e) => write!(f, "{e}"),
        }
    }
}

impl From<tokio::sync::mpsc::error::SendError<ClientEvent>> for ClientError {
    fn from(_value: tokio::sync::mpsc::error::SendError<ClientEvent>) -> Self {
        Self::ChannelClosed
    }
}

pub enum ClientEvent {
    ConnectionRequest(SocketAddr),
    Connected(SocketAddr),
    Listening(SocketAddr),
    Disconnected(SocketAddr),
    PingPongStarted,
    PingPongSuccessful,
}

impl std::fmt::Display for ClientEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientEvent::ConnectionRequest(socket_addr) => {
                write!(f, "Received connection request from {socket_addr}")
            }
            ClientEvent::Connected(socket_addr) => write!(f, "Client {socket_addr} connected"),
            ClientEvent::Listening(socket_addr) => write!(f, "Listening on {socket_addr}"),
            ClientEvent::Disconnected(socket_addr) => {
                write!(f, "Client {socket_addr} disconnected")
            }
            ClientEvent::PingPongStarted => write!(f, "Sent PING request"),
            ClientEvent::PingPongSuccessful => write!(f, "PING PONG successfully completed"),
        }
    }
}

struct ClientHandlerArgs {
    conn_alpn: Arc<[u8]>,
    server_nodeid: NodeId,
    stream: TcpStream,
    remote: SocketAddr,
    events: Sender<ClientEvent>,
    endpoint: Arc<Endpoint>,
}

#[allow(clippy::too_many_arguments)]
pub async fn client(
    listen_ip: SocketAddr,
    node_id: &str,
    conn_alpn: &str,
    ping_alpn: &str,
    ping_msg: &str,
    pong_msg: &str,
    events: Sender<ClientEvent>,
    errors: Sender<ClientError>,
) -> Result<(), ClientError> {
    let conn_alpn: Arc<[u8]> = Arc::from(conn_alpn.as_bytes());
    let server_nodeid = NodeId::from_str(node_id)
        .map_err(|e| ClientError::InvalidNodeId(format!("Error when parsing key: {e}")))?;

    let listener = TcpListener::bind(listen_ip)
        .await
        .map_err(|e| ClientError::ListenError(format!("Failed to start local tcplistener: {e}")))?;
    let local_addr = listener.local_addr().map_err(|e| {
        ClientError::ListenError(format!(
            "Failed to get local server ip from local_addr: {e}"
        ))
    })?;
    let endpoint = Endpoint::builder()
        .discovery_n0()
        .bind()
        .await
        .map_err(|e| {
            ClientError::EndpointSetup(format!("Failed to create client iroh endpoint: {e}"))
        })?;
    let endpoint = Arc::new(endpoint);
    handle_client_ping(
        &endpoint,
        server_nodeid,
        ping_alpn,
        ping_msg,
        pong_msg,
        &events,
    )
    .await?;
    events.send(ClientEvent::Listening(local_addr)).await?;
    loop {
        let fut = listener.accept().await;

        let (client, clientaddr) = match fut {
            Ok(conn) => conn,
            Err(e) => {
                errors
                    .send(ClientError::ConnectionError(format!(
                        "Error accepting TCP connection from client: {e}"
                    )))
                    .await
                    .unwrap();
                continue;
            }
        };
        let endpoint = endpoint.clone();
        let conn_alpn = conn_alpn.clone();
        let events = events.clone();
        let errors = errors.clone();
        tokio::task::spawn(async move {
            let args = ClientHandlerArgs {
                conn_alpn,
                server_nodeid,
                stream: client,
                remote: clientaddr,
                events,
                endpoint,
            };
            match client_handler(args).await {
                Ok(()) => (),
                Err(e) => {
                    errors.send(e).await.unwrap();
                }
            }
        });
    }
}

async fn client_handler(args: ClientHandlerArgs) -> Result<(), ClientError> {
    let ClientHandlerArgs {
        server_nodeid,
        conn_alpn,
        mut stream,
        remote: clientaddr,
        events,
        endpoint,
    } = args;

    events
        .send(ClientEvent::ConnectionRequest(clientaddr))
        .await?;
    let (mut client_recv, mut client_send) = stream.split();
    let server_conn = endpoint
        .connect(server_nodeid, conn_alpn.as_ref())
        .await
        .map_err(|e| {
            ClientError::ConnectionError(format!(
                "Error when establishing connection with server: {e}"
            ))
        })?;

    let (mut server_send, mut server_recv) = server_conn.open_bi().await.map_err(|e| {
        ClientError::ConnectionError(format!(
            "Error when establishing bidirectional iroh channel with server: {e}"
        ))
    })?;
    events.send(ClientEvent::Connected(clientaddr)).await?;
    select! {
        res = tokio::io::copy(&mut client_recv, &mut server_send) => {
            match res {
                Ok(_) => {
                    server_send.finish().unwrap();
                    if let Err(e) = server_send.stopped().await {
                        return Err(ClientError::UnexpectedDisconnect(format!("Sending shutdown signal to server failed: {e}")))
                    }
                    events.send(ClientEvent::Disconnected(clientaddr)).await?;
                },
                Err(e) => {
                    events.send(ClientEvent::Disconnected(clientaddr)).await?;
                    return Err(ClientError::UnexpectedDisconnect(format!("Client {clientaddr} disconnected with err: {e}")));
                },
            }
        },
        res = tokio::io::copy(&mut server_recv, &mut client_send) => {
            match res {
                Ok(_) => {
                    if let Err(e) = server_send.shutdown().await {
                        return Err(ClientError::UnexpectedDisconnect(format!("Sending shutdown signal to server failed: {e}")));
                    };
                    events.send(ClientEvent::Disconnected(clientaddr)).await?;
                },
                Err(e) => {
                    events.send(ClientEvent::Disconnected(clientaddr)).await?;
                    return Err(ClientError::UnexpectedDisconnect(format!("Client {clientaddr} disconnected with error: {e}")));
                },
            }
        },
    };
    Ok(())
}

async fn handle_client_ping(
    endpoint: &Endpoint,
    server_nodeid: NodeId,
    ping_alpn: &str,
    ping_msg: &str,
    pong_msg: &str,
    events: &Sender<ClientEvent>,
) -> Result<(), ClientError> {
    events.send(ClientEvent::PingPongStarted).await?;
    let conn = endpoint
        .connect(server_nodeid, ping_alpn.as_ref())
        .await
        .map_err(|e| {
            ClientError::Ping(format!("Error establishing PING connection to server: {e}"))
        })?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| {
        ClientError::Ping(format!(
            "Error establishing bidirectional PING streams with server: {e}"
        ))
    })?;

    if let Err(e) = send.write_all(ping_msg.as_bytes()).await {
        return Err(ClientError::Ping(format!(
            "Error sending PING message to server: {e}"
        )));
    }
    // Should never fail according to docs
    send.finish().unwrap();
    let data = recv
        .read_to_end(pong_msg.len())
        .await
        .map_err(|e| ClientError::Ping(format!("Error receiving PONG message from server: {e}")))?;
    if data != pong_msg.as_bytes() {
        return Err(ClientError::Ping(
            "Invalid PONG message received from server".to_string(),
        ));
    }
    events.send(ClientEvent::PingPongSuccessful).await?;
    Ok(())
}

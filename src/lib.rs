#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod wordlist;

pub const PROTOCOL_VERSION: u64 = 3;

#[derive(Debug, Error)]
pub enum SharedError {
    #[error("Error when decoding Varint")]
    VarIntDecode,
    #[error("Error when serializing: {0}")]
    DataSerialize(postcard::Error),
    #[error("Error when deserializing: {0}")]
    DataDeserialize(postcard::Error),
    #[error("Error when writing to stream: {0}")]
    Write(std::io::Error),
    #[error("Error when reading from stream: {0}")]
    Read(std::io::Error),
    #[error("Received data larger than Buf size")]
    BufSize,
    #[error("Invalid packet received: ")]
    InvalidPacket(String),
}

/// Messages transferred through the initial TLS stream between proxy and server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Proxy request to server to echo back these bytes
    HeartBeat([u8; 32]),
    /// Request from proxy to server telling server to open the TCP stream that the client connection will
    /// be proxied through.
    NewClient(u128),
}

/// Response to init connection sent from proxy to server telling server the domain it is assigned
/// and the proxy server's protocol number. This protocol number will be incremented every time the protocol changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitResponse {
    /// The url that was assigned to the Minecraft server. (ex: <word>-<word>-<word>.mineshare.dev)
    pub domain: String,
    /// Protocol version for the proxy server, not Minecraft protocol version
    pub protocol_version: u64,
}

impl InitResponse {
    #[must_use]
    pub fn new(domain: String, protocol_version: u64) -> Self {
        Self {
            domain,
            protocol_version,
        }
    }
}

/// An IP Address + Port
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Addr(pub SocketAddr);

/// A simple hello string from server to proxy server to validate that the server is a valid mineshare server
/// and an optional domain requested by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello<'a> {
    pub hello_string: &'a str,
    pub requested_domain: Option<&'a str>,
}

pub const HELLO_STRING: &str = "mineshare";

impl<'a> ServerHello<'a> {
    #[must_use]
    pub fn new(requested_domain: Option<&'a str>) -> ServerHello<'a> {
        ServerHello {
            hello_string: HELLO_STRING,
            requested_domain,
        }
    }
}

impl<'a> StreamHelper<'a> for ServerHello<'a> {}
impl StreamHelper<'_> for Addr {}
impl StreamHelper<'_> for Message {}
impl StreamHelper<'_> for InitResponse {}

pub trait StreamHelper<'a>: Serialize + Deserialize<'a> + Send + Sync {
    fn encode<S: AsyncWrite + Unpin + Send>(
        &self,
        w: &mut S,
        buf: &mut [u8],
    ) -> impl std::future::Future<Output = Result<usize, SharedError>> + Send {
        async {
            let slice = postcard::to_slice(self, buf).map_err(SharedError::DataSerialize)?;
            w.write_u32(slice.len() as u32)
                .await
                .map_err(SharedError::Write)?;
            w.write_all(slice).await.map_err(SharedError::Write)?;
            w.flush().await.map_err(SharedError::Write)?;
            // Len of "len" = 4
            Ok(slice.len() + 4)
        }
    }
    fn decode<S: AsyncRead + Unpin + Send>(
        r: &mut S,
        buf: &'a mut [u8],
    ) -> impl std::future::Future<Output = Result<Self, SharedError>> + Send {
        async {
            let len = r.read_u32().await.map_err(SharedError::Read)?;
            let len = len as usize;
            if len > buf.len() {
                return Err(SharedError::BufSize);
            }
            let _read = r
                .read_exact(&mut buf[..len])
                .await
                .map_err(SharedError::Read)?;
            let decoded: Self =
                postcard::from_bytes(&buf[..len]).map_err(SharedError::DataDeserialize)?;
            Ok(decoded)
        }
    }
}

/// Try and parse the hostname from the initial client packet from a Minecraft client to a Minecraft server
/// Returns `Ok(Some(&[u8]))` if hostname was parsed successfully
/// Returns `Ok(None)` if it needs more data
/// Returns `Err(e)` if an error happened or data is malformed
pub fn try_parse_init_packet(buf: &[u8]) -> Result<Option<&[u8]>, SharedError> {
    let mut inner_cursor = 0;
    let Some((_len, read)) = varint::decode_varint(buf)? else {
        return Ok(None);
    };
    inner_cursor += read;
    let Some((packet_id, read)) = varint::decode_varint(&buf[inner_cursor..])? else {
        return Ok(None);
    };
    inner_cursor += read;
    if packet_id != 0 {
        // Handshaking protocol ID should be 0
        return Err(SharedError::InvalidPacket(
            "Client sent invalid protocol ID".into(),
        ));
    }
    let Some((_protocol_version, read)) = varint::decode_varint(&buf[inner_cursor..])? else {
        return Ok(None);
    };
    inner_cursor += read;
    let Some((strlen, read)) = varint::decode_varint(&buf[inner_cursor..])? else {
        return Ok(None);
    };
    inner_cursor += read;
    if strlen > 256 {
        return Err(SharedError::InvalidPacket(format!(
            "Hostname length too long: {strlen} > 256"
        )));
    }
    if buf[inner_cursor..].len() < strlen as usize {
        // Haven't received enough data
        return Ok(None);
    }
    let hostname = &buf[inner_cursor..inner_cursor + strlen as usize];
    Ok(Some(hostname))
}

/// Utilities for encoding and decoding varints
pub mod varint {
    use super::SharedError;
    /// Encode a u64 as a varint.
    #[must_use]
    pub fn encode_varint(mut value: u64) -> ([u8; 10], usize) {
        let mut index = 0;
        let mut buf = [0u8; 10];
        while value >= 0x80 {
            buf[index] = (value as u8 & 0x7F) | 0x80;
            index += 1;
            value >>= 7;
        }
        buf[index] = value as u8;
        index += 1;
        (buf, index)
    }
    /// Decode a varint from a buffer
    pub fn decode_varint(buf: &[u8]) -> Result<Option<(u64, usize)>, SharedError> {
        let mut result = 0;
        for i in 0..10 {
            let byte = match buf.get(i) {
                Some(b) => *b,
                None => return Ok(None),
            };
            result |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(Some((result, i + 1)));
            }
        }
        Err(SharedError::VarIntDecode)
    }
}

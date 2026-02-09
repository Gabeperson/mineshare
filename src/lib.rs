#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::similar_names)]

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub mod client_impls;
pub mod wordlist;

pub const PROTOCOL_VERSION: u64 = 4;

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
pub struct Addr {
    pub addr: SocketAddr,
    pub username: Option<String>,
}

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

#[derive(Debug)]
struct MCString<const N: usize>(pub String);

impl<const N: usize> MCString<N> {
    async fn decode(
        reader: &mut (impl AsyncRead + Unpin),
        data: &mut Vec<u8>,
    ) -> Result<(Self, usize), SharedError> {
        let (len, len_len) = varint::decode_varint(reader, data).await?;
        let len = len as usize;
        if len > N {
            return Err(SharedError::InvalidPacket(
                "Length of string larger than max".into(),
            ));
        }
        let data_len = data.len();
        data.resize(data_len + len, 0);
        let _str_len = reader
            .read_exact(&mut data[data_len..])
            .await
            .map_err(SharedError::Read)?;
        let s = str::from_utf8(&data[data_len..]).map_err(|_| {
            SharedError::InvalidPacket("Expected String, but found non-utf8".into())
        })?;

        Ok((MCString(s.to_owned()), len + len_len))
    }
}

pub async fn try_parse_init_packet(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut Vec<u8>,
) -> Result<(String, Option<String>), SharedError> {
    let mut reader = BufReader::new(stream);

    let (pkt_len1, _len) = varint::decode_varint(&mut reader, data).await?;
    let (pkt_id1, pkt_id1_len) = varint::decode_varint(&mut reader, data).await?;
    if pkt_id1 != 0 {
        return Err(SharedError::InvalidPacket(
            "Client sent invalid protocol ID (Handshake)".into(),
        ));
    }
    let (_mc_protocol_version, mc_protocol_version_len) =
        varint::decode_varint(&mut reader, data).await?;
    let (hostname, hostname_len) = MCString::<255>::decode(&mut reader, data).await?;
    let (port, port_len) = (
        reader.read_u16().await.map_err(SharedError::Read)?,
        u16::BITS / 8,
    );
    data.write_u16(port).await.expect("Write to vec");
    let port_len = port_len as usize;
    let (intent, intent_len) = varint::decode_varint(&mut reader, data).await?;
    let pkt1_total_len =
        pkt_id1_len + mc_protocol_version_len + hostname_len + port_len + intent_len;
    if pkt_len1 as usize != pkt1_total_len {
        return Err(SharedError::InvalidPacket(
            "Received packet size different from packet size in header".into(),
        ));
    }
    let username = match intent {
        // Status, so we won't have a "Login Start" packet.
        1 => None,
        // Login / Transfer
        2 | 3 => {
            let (_pkt_len2, _len) = varint::decode_varint(&mut reader, data).await?;
            let (pkt_id2, _pkt_id2_len) = varint::decode_varint(&mut reader, data).await?;
            if pkt_id2 != 0x00 {
                return Err(SharedError::InvalidPacket(
                    "Client sent invalid protocol ID (Login)".into(),
                ));
            }
            let (username, _username_len) = MCString::<16>::decode(&mut reader, data).await?;
            // We can't verify anything else here, like we did with the initial handshake packet.
            // This is because for some reason, Mojang changed the packet representation FOUR TIMES, AND IN
            // COMPLETELY INCOMPATIBLE WAYS IF WE WANT TO VERIFY PACKET LENGTH. WHY MOJANG? WHY?
            // Thankfully all of them have username as a string and as the first field, so can parse that as we have done above.
            // So since we want to stay protocol-id agnostic we just accept that and ignore the rest.
            Some(username.0)
        }
        _ => return Err(SharedError::InvalidPacket("Invalid intent".into())),
    };

    data.extend(reader.buffer());

    Ok((hostname.0, username))
}

/// Utilities for encoding and decoding varints
pub mod varint {
    use tokio::io::{AsyncRead, AsyncReadExt as _};

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
    /// Decode a varint from a stream (while also writing read bytes to a buffer)
    pub async fn decode_varint(
        stream: &mut (impl AsyncRead + Unpin),
        buf: &mut Vec<u8>,
    ) -> Result<(u64, usize), SharedError> {
        let mut result = 0;
        for i in 0..10 {
            let byte = stream.read_u8().await.map_err(SharedError::Read)?;
            buf.push(byte);
            result |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok((result, i + 1));
            }
        }
        Err(SharedError::VarIntDecode)
    }
}

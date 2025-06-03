#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]

use bincode::{Decode, Encode};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::num::NonZero;
use tracing::warn;

pub mod wordlist;

pub const LIMIT_MEGABYTE_PER_SECOND: NonZero<u32> = NonZero::new(128 * 1024).unwrap();
pub const LIMIT_BURST: NonZero<u32> = NonZero::new(256 * 1024).unwrap();

#[derive(Debug, Clone, Encode, Decode)]
pub enum Message {
    HeartBeat([u8; 32]),
    HeartBeatEcho([u8; 32]),
    NewClient(u128),
}

pub fn try_parse_init_packet(
    buf: &[u8],
    addr: SocketAddr,
) -> Result<Option<&[u8]>, std::io::Error> {
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
        warn!("client {addr} sent invalid packet protocol ID");
        return Err(ErrorKind::InvalidData.into());
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
        warn!("Received packet with hostname len set to {strlen}");
        return Err(ErrorKind::InvalidData.into());
    }
    if buf[inner_cursor..].len() < strlen as usize {
        // Haven't received enough data
        return Ok(None);
    }
    let hostname = &buf[inner_cursor..inner_cursor + strlen as usize];
    Ok(Some(hostname))
}

pub mod varint {
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

    pub fn decode_varint(buf: &[u8]) -> Result<Option<(u64, usize)>, std::io::Error> {
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
        Err(std::io::ErrorKind::InvalidData.into())
    }
}

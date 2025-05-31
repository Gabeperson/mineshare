pub mod wordlist;

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::num::NonZero;

use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::InMemoryState;
use governor::state::NotKeyed;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tracing::info;
use tracing::warn;

pub const RECV_BUF_SIZE: usize = 128 * 1024;
pub const MUX_RECV_BUF_SIZE: usize = 256 * 1024;
pub const TIMEOUT_DURATION_SECS: f32 = 15.;
pub const SEND_BUF: usize = 8 * 1024;

pub const LIMIT_MEGABYTE_PER_SECOND: NonZero<u32> = NonZero::new(128 * 1024).unwrap();
pub const LIMIT_BURST: NonZero<u32> = NonZero::new(256 * 1024).unwrap();

pub const BUF_PER_CONN: usize = 256 * 1024; // yamux default
pub const MAX_CONN_COUNT: usize = 20;
pub const TOTAL_BUF_SIZE: usize = BUF_PER_CONN * MAX_CONN_COUNT;

#[derive(Clone, Debug)]
pub enum Message<'a> {
    DomainDecided(&'a [u8]),
    Disconnect(u64),
    Connect(u64),
    Data(u64, &'a [u8]),
    HeartBeat,
    Acknowledged(u64, usize),
}

impl<'a> Message<'a> {
    fn get_discriminant(&self) -> u8 {
        match self {
            Message::DomainDecided(_) => 0,
            Message::Disconnect(_) => 1,
            Message::Connect(_) => 2,
            Message::Data(_, _) => 3,
            Message::HeartBeat => 4,
            Message::Acknowledged(_, _) => 5,
        }
    }
    pub async fn encode<S: AsyncWrite + Unpin>(self, writer: &mut S) -> Result<(), std::io::Error> {
        writer.write_u8(self.get_discriminant()).await?;
        match self {
            Message::DomainDecided(d) => {
                let (buf, len) = varint::encode_varint(d.len() as u64);
                writer.write_all(&buf[..len]).await?;
                writer.write_all(d).await?;
                Ok(())
            }
            Message::Connect(n) | Message::Disconnect(n) => {
                writer.write_u64(n).await?;
                Ok(())
            }
            Message::Acknowledged(id, amt) => {
                let (buf, len) = varint::encode_varint(id);
                writer.write_all(&buf[..len]).await?;
                let (buf, len) = varint::encode_varint(amt as u64);
                writer.write_all(&buf[..len]).await?;
                Ok(())
            }
            Message::Data(id, bytes) => {
                let (buf, len) = varint::encode_varint(id);
                writer.write_all(&buf[..len]).await?;
                let (buf, len) = varint::encode_varint(bytes.len() as u64);
                writer.write_all(&buf[..len]).await?;
                writer.write_all(bytes).await?;
                Ok(())
            }
            Message::HeartBeat => Ok(()),
        }
    }
    pub fn decode(buf: &'a [u8]) -> Result<Option<(Message<'a>, usize)>, std::io::Error> {
        let mut cursor = 0;
        let discriminant = match buf.get(cursor) {
            Some(d) => *d,
            None => return Ok(None),
        };
        cursor += 1;
        match discriminant {
            0 => {
                let (len, read) = match varint::decode_varint(&buf[cursor..]) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let len = len as usize;
                cursor += read;
                // Domain should not be >= 512 bytes
                if len >= 512 {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                if let Some(slice) = buf.get(cursor..cursor + len) {
                    Ok(Some((Message::DomainDecided(slice), cursor + len)))
                } else {
                    Ok(None)
                }
            }
            1 => {
                if let Some(n) = buf.get(cursor..cursor + 8) {
                    return Ok(Some((
                        Message::Disconnect(u64::from_be_bytes(n.try_into().unwrap())),
                        cursor + 8,
                    )));
                }
                Ok(None)
            }
            2 => {
                if let Some(n) = buf.get(cursor..cursor + 8) {
                    return Ok(Some((
                        Message::Connect(u64::from_be_bytes(n.try_into().unwrap())),
                        cursor + 8,
                    )));
                }
                Ok(None)
            }
            3 => {
                let (id, read) = match varint::decode_varint(&buf[cursor..]) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                };
                cursor += read;
                let (len, read) = match varint::decode_varint(&buf[cursor..]) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let len = len as usize;
                cursor += read;
                if len >= RECV_BUF_SIZE {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                if let Some(slice) = buf.get(cursor..cursor + len) {
                    Ok(Some((Message::Data(id, slice), cursor + len)))
                } else {
                    Ok(None)
                }
            }
            4 => Ok(Some((Message::HeartBeat, 1))),
            5 => {
                let (id, read) = match varint::decode_varint(&buf[cursor..]) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                };
                cursor += read;
                let (amt, read) = match varint::decode_varint(&buf[cursor..]) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                };
                cursor += read;
                Ok(Some((Message::Acknowledged(id, amt as usize), cursor)))
            }
            _ => Err(std::io::ErrorKind::InvalidData.into()),
        }
    }
}

pub fn try_parse_init_packet<'a>(
    buf: &'a [u8],
    addr: SocketAddr,
    base_domain: &[u8],
) -> Result<Option<&'a [u8]>, std::io::Error> {
    let mut inner_cursor = 0;
    let (_len, read) = match varint::decode_varint(buf) {
        Ok(Some(res)) => res,
        Ok(None) => return Ok(None),
        Err(_e) => {
            info!("client {addr} sent invalid packet length varint (cannot parse)");
            return Err(ErrorKind::InvalidData.into());
        }
    };
    inner_cursor += read;
    let (packet_id, read) = match varint::decode_varint(&buf[inner_cursor..]) {
        Ok(Some(res)) => res,
        Ok(None) => return Ok(None),
        Err(_e) => {
            info!("client {addr} sent invalid packet id varint (cannot parse)");
            return Err(ErrorKind::InvalidData.into());
        }
    };
    inner_cursor += read;
    if packet_id != 0 {
        // Handshaking protocol ID should be 0
        info!("client {addr} sent invalid packet protocol ID");
        return Err(ErrorKind::InvalidData.into());
    }
    let (_protocol_version, read) = match varint::decode_varint(&buf[inner_cursor..]) {
        Ok(Some(res)) => res,
        Ok(None) => return Ok(None),
        Err(_e) => {
            info!("client {addr} sent invalid protocol version varint (cannot parse)");
            return Err(ErrorKind::InvalidData.into());
        }
    };
    inner_cursor += read;
    let (strlen, read) = match varint::decode_varint(&buf[inner_cursor..]) {
        Ok(Some(res)) => res,
        Ok(None) => return Ok(None),
        Err(_e) => {
            info!("client {addr} sent invalid Host string length");
            return Err(ErrorKind::InvalidData.into());
        }
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
    let access = if let Some(prefix) = hostname.strip_suffix(base_domain) {
        if prefix.is_empty() {
            info!("client {addr} sent empty hostname");
            return Err(ErrorKind::InvalidData.into());
        }
        prefix
    } else {
        info!(
            "client {addr} connected via non-base domain URL: `{}`",
            String::from_utf8_lossy(hostname)
        );
        return Err(ErrorKind::InvalidData.into());
    };
    Ok(Some(access))
}

pub mod varint {
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
            result |= ((byte & 0x7f) as u64) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(Some((result, i + 1)));
            }
        }
        Err(std::io::ErrorKind::InvalidData.into())
    }
}

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

// #[derive(Debug)]
// pub(crate) struct SingleRecvConn {
//     pub(crate) stream: TcpStream,
//     pub(crate) limiter: Arc<Limiter>,
//     pub(crate) buf: Box<[u8; 8096]>,
//     pub(crate) buf_filled: Option<NonZeroU32>,
//     pub(crate) id: u64,
// }

// impl SingleRecvConn {
//     pub(crate) fn recv(&mut self) -> SingleConnReceiveDataFuture {
//         SingleConnReceiveDataFuture { client: Some(self) }
//     }
// }

// #[derive(Debug)]
// pub(crate) struct SingleConnReceiveDataFuture<'a> {
//     // Lifetime workaround
//     client: Option<&'a mut SingleRecvConn>,
// }

// impl<'a> Future for SingleConnReceiveDataFuture<'a> {
//     type Output = Result<usize, std::io::Error>;

//     fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
//         let client = self.client.take().unwrap();
//         match client.buf_filled {
//             Some(n) => {
//                 {
//                     let fut = client.limiter.until_n_ready(n);
//                     let fut = pin!(fut);
//                     match fut.poll(cx) {
//                         Poll::Ready(res) => {
//                             // Since we use 8KB buffer this should always succeed
//                             debug_assert!(res.is_ok());
//                         }
//                         Poll::Pending => return Poll::Pending,
//                     }
//                 }
//                 // Buffer is filled && we have enough quota to send this section
//                 // We don't update buf_filled here because we don't know if the other side of the
//                 // multiplexed stream has enough quota to receive
//                 client.buf_filled = None;
//                 let n = n.get() as usize;
//                 Poll::Ready(Ok(n))
//             }
//             None => {
//                 let filled = {
//                     let stream = &mut client.stream;
//                     pin_mut!(stream);
//                     let mut buffer = ReadBuf::new(&mut *client.buf);
//                     match stream.poll_read(cx, &mut buffer) {
//                         Poll::Ready(v) => {
//                             if let Err(e) = v {
//                                 // The TCP connection disconnected, bubble up to handle it
//                                 return Poll::Ready(Err(e));
//                             }
//                             buffer.filled_mut().len()
//                         }
//                         // Stream doesn't have any extra data, we just keep polling
//                         Poll::Pending => return Poll::Pending,
//                     }
//                 };
//                 let filled = filled as u32; // We use an 8KB buffer so it's impossible for this to get above u32 limit.
//                 if filled == 0 {
//                     // EOF, pass it on
//                     return Poll::Ready(Ok(0));
//                 }
//                 let filled = NonZeroU32::new(filled).unwrap();
//                 let fut = client.limiter.until_n_ready(filled);
//                 let fut = pin!(fut);
//                 match fut.poll(cx) {
//                     Poll::Ready(res) => {
//                         // Since we use 8kb buffer this should always succeed
//                         debug_assert!(res.is_ok());
//                     }
//                     Poll::Pending => {
//                         // We received data, but don't have enough quota to send it.
//                         // So we update the buffer filled amount and wait until enough quota is filled
//                         client.buf_filled = Some(filled);
//                         return Poll::Pending;
//                     }
//                 }
//                 // We have data, and have enough quota to send it!
//                 Poll::Ready(Ok(filled.get() as usize))
//             }
//         }
//     }
// }

// #[derive(Debug)]
// pub(crate) struct ArrayPollSingle<'a> {
//     pub(crate) arr: Option<&'a mut [SingleRecvConn]>,
//     pub(crate) index: Option<&'a mut usize>,
// }

// impl<'a> Future for ArrayPollSingle<'a> {
//     type Output = (Result<usize, std::io::Error>, usize);

//     fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
//         let arr = self.arr.take().unwrap();
//         let index = self.index.take().unwrap();
//         *index %= arr.len();
//         for _ in 0..arr.len() {
//             {
//                 let fut = arr[*index].recv();
//                 let fut = pin!(fut);
//                 match fut.poll(cx) {
//                     Poll::Ready(res) => return Poll::Ready((res, *index)),
//                     Poll::Pending => {}
//                 }
//                 *index += 1;
//                 *index %= arr.len();
//             }
//         }
//         Poll::Pending
//     }
// }

// #[derive(Debug)]
// pub(crate) struct RecvBuffer {
//     pub(crate) inner: [u8; RECV_BUF_SIZE],
//     pub(crate) end: usize,
// }
// impl Default for RecvBuffer {
//     fn default() -> Self {
//         Self::new()
//     }
// }
// impl RecvBuffer {
//     pub(crate) fn new() -> Self {
//         Self {
//             inner: [0; RECV_BUF_SIZE],
//             end: 0,
//         }
//     }
//     pub(crate) fn len(&self) -> usize {
//         self.end - 1
//     }
//     pub(crate) fn is_empty(&self) -> bool {
//         self.len() == 0
//     }
//     pub(crate) fn available(&self) -> usize {
//         RECV_BUF_SIZE - self.len()
//     }
//     pub(crate) fn to_write(&self) -> usize {
//         self.len().min(8096)
//     }
//     pub(crate) fn remaining(&self) -> usize {
//         self.len() - self.to_write()
//     }
//     pub(crate) fn wrote(&mut self, bytes: usize) {
//         let remaining = self.remaining();
//         self.inner.copy_within(bytes..bytes + remaining, 0);
//         self.end = remaining;
//     }
//     pub(crate) fn write(&mut self, slice: &[u8]) {
//         self.inner[self.end..self.end + slice.len()].copy_from_slice(slice);
//         self.end += slice.len();
//     }
// }

// #[derive(Debug)]
// pub(crate) struct RecvMux {
//     pub(crate) limiter: Arc<Limiter>,
//     pub(crate) buf: RecvBuffer,
//     pub(crate) id: usize,
// }

// #[derive(Debug)]
// pub(crate) struct RecvMuxFuture<'a> {
//     // Lifetime workaround
//     conn: Option<&'a mut RecvMux>,
// }

// impl<'a> Future for RecvMuxFuture<'a> {
//     type Output = usize;

//     fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
//         let conn = self.conn.take().unwrap();
//         let len = conn.buf.len();
//         if len == 0 {
//             // if there's nothing to transfer, we don't have anything to do :)
//             return Poll::Pending;
//         }
//         let to_write = conn.buf.to_write();
//         let fut = conn
//             .limiter
//             // We just cast the len, because the max length of the buffer will NEVER be > 4gb.
//             .until_n_ready(NonZeroU32::new(to_write as u32).unwrap());
//         let fut = pin!(fut);
//         match fut.poll(cx) {
//             Poll::Ready(res) => {
//                 // Since we use small buffer this should always succeed
//                 debug_assert!(res.is_ok());
//                 Poll::Ready(conn.id)
//             }
//             Poll::Pending => Poll::Pending,
//         }
//     }
// }

// #[derive(Debug)]
// pub(crate) struct ArrayPollMux<'a> {
//     pub(crate) arr: Option<&'a mut [RecvMux]>,
//     pub(crate) index: Option<&'a mut usize>,
// }

// impl<'a> Future for ArrayPollMux<'a> {
//     type Output = (usize, usize);

//     fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
//         let arr = self.arr.take().unwrap();
//         let index = self.index.take().unwrap();
//         *index %= arr.len();
//         for _ in 0..arr.len() {
//             {
//                 let fut = RecvMuxFuture {
//                     conn: Some(&mut arr[*index]),
//                 };
//                 let fut = pin!(fut);
//                 // blocked on https://github.com/rust-lang/rust/issues/54663
//                 match fut.poll(cx) {
//                     Poll::Ready(res) => return Poll::Ready((res, *index)),
//                     Poll::Pending => {}
//                 }
//                 *index += 1;
//                 *index %= arr.len();
//             }
//         }
//         Poll::Pending
//     }
// }

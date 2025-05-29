pub mod wordlist;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use futures::StreamExt;
use futures::pin_mut;
use futures::stream::FuturesUnordered;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::InMemoryState;
use governor::state::NotKeyed;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::net::TcpStream;

pub const RECV_BUF_SIZE: usize = 256 * 1024;
pub const TIMEOUT_DURATION_SECS: f32 = 15.;

#[derive(Clone, Debug)]
pub enum Message<'a> {
    DomainDecided(&'a [u8]),
    Disconnect(u64),
    Connect(u64),
    Data(&'a [u8]),
    HeartBeat,
    Acknowledged(u64),
}

impl<'a> Message<'a> {
    fn get_discriminant(&self) -> u8 {
        match self {
            Message::DomainDecided(_) => 0,
            Message::Disconnect(_) => 1,
            Message::Connect(_) => 2,
            Message::Data(_) => 3,
            Message::HeartBeat => 4,
            Message::Acknowledged(_) => 5,
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
            Message::Connect(n) | Message::Disconnect(n) | Message::Acknowledged(n) => {
                writer.write_u64(n).await?;
                Ok(())
            }
            Message::Data(bytes) => {
                let (buf, len) = varint::encode_varint(bytes.len() as u64);
                writer.write_all(&buf[..len]).await?;
                writer.write_all(bytes).await?;
                Ok(())
            }
            Message::HeartBeat => Ok(()),
        }
    }
    pub async fn decode<S: AsyncRead + Unpin>(
        reader: &mut S,
        buf: &'a mut [u8],
    ) -> Result<Message<'a>, std::io::Error> {
        let discriminant = reader.read_u8().await?;
        match discriminant {
            0 => {
                let len = varint::decode_varint(reader).await? as usize;
                if len >= 512 {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                let _len = reader.read_exact(&mut buf[..len]).await?;
                Ok(Message::DomainDecided(&buf[..len]))
            }
            1 => {
                let id = reader.read_u64().await?;
                Ok(Message::Disconnect(id))
            }
            2 => {
                let id = reader.read_u64().await?;
                Ok(Message::Connect(id))
            }
            3 => {
                let len = varint::decode_varint(reader).await? as usize;
                if len >= RECV_BUF_SIZE {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                let _len = reader.read_exact(&mut buf[..len as usize]).await?;
                Ok(Message::DomainDecided(&buf[..len as usize]))
            }
            4 => Ok(Message::HeartBeat),
            _ => Err(std::io::ErrorKind::InvalidData.into()),
        }
    }
}

pub mod varint {
    use tokio::io::AsyncRead;
    use tokio::io::AsyncReadExt;

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

    pub async fn decode_varint<S: AsyncRead + Unpin>(
        reader: &mut S,
    ) -> Result<u64, std::io::Error> {
        let mut result = 0;
        for i in 0..10 {
            let byte = reader.read_u8().await?;
            result |= ((byte & 0x7f) as u64) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(std::io::ErrorKind::InvalidData.into())
    }
}

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[derive(Debug)]
pub struct SingleRecvConn {
    pub stream: TcpStream,
    pub limiter: Arc<Limiter>,
    pub buf: Box<[u8; 8096]>,
    pub buf_filled: Option<NonZeroU32>,
    pub id: usize,
}

impl SingleRecvConn {
    pub fn recv(&mut self) -> ConnReceiveDataFuture {
        ConnReceiveDataFuture { client: Some(self) }
    }
}

#[derive(Debug)]
pub struct ConnReceiveDataFuture<'a> {
    // Lifetime workaround
    client: Option<&'a mut SingleRecvConn>,
}

impl<'a> Future for ConnReceiveDataFuture<'a> {
    type Output = (Result<&'a [u8], std::io::Error>, usize);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let client = self.client.take().unwrap();
        match client.buf_filled {
            Some(n) => {
                {
                    let fut = client.limiter.until_n_ready(n);
                    let fut = pin!(fut);
                    match fut.poll(cx) {
                        Poll::Ready(res) => {
                            // Since we use 8KB buffer this should always succeed
                            debug_assert!(res.is_ok());
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                // Buffer is filled && we have enough quota to send this section
                // We don't update buf_filled here because we don't know if the other side of the
                // multiplexed stream has enough quota to receive
                let n = n.get() as usize;
                Poll::Ready((Ok(&client.buf[..n]), client.id))
            }
            None => {
                let filled = {
                    let stream = &mut client.stream;
                    pin_mut!(stream);
                    let mut buffer = ReadBuf::new(&mut *client.buf);
                    match stream.poll_read(cx, &mut buffer) {
                        Poll::Ready(v) => {
                            if let Err(e) = v {
                                // The TCP connection disconnected, bubble up to handle it
                                return Poll::Ready((Err(e), client.id));
                            }
                            buffer.filled_mut().len()
                        }
                        // Stream doesn't have any extra data, we just keep polling
                        Poll::Pending => return Poll::Pending,
                    }
                };
                let filled = filled as u32; // We use an 8KB buffer so it's impossible for this to get above u32 limit.
                if filled == 0 {
                    // EOF, pass it on
                    return Poll::Ready((Ok(&[]), client.id));
                }
                let filled = NonZeroU32::new(filled).unwrap();
                let fut = client.limiter.until_n_ready(filled);
                let fut = pin!(fut);
                match fut.poll(cx) {
                    Poll::Ready(res) => {
                        // Since we use 8kb buffer this should always succeed
                        debug_assert!(res.is_ok());
                    }
                    Poll::Pending => {
                        // We received data, but don't have enough quota to send it.
                        // So we update the buffer filled amount and wait until enough quota is filled
                        client.buf_filled = Some(filled);
                        return Poll::Pending;
                    }
                }
                // We have data, and have enough quota to send it!
                Poll::Ready((Ok(&client.buf[..filled.get() as usize]), client.id))
            }
        }
    }
}

#[derive(Debug)]
pub struct PendingIfZero<Fut: Future> {
    pub unordered: FuturesUnordered<Fut>,
}

impl<Fut: Future> PendingIfZero<Fut> {
    pub fn new(unordered: FuturesUnordered<Fut>) -> Self {
        Self { unordered }
    }
}

impl<Fut: Future> Future for PendingIfZero<Fut> {
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let Poll::Ready(opt) = self.unordered.poll_next_unpin(cx) else {
            // futures unordered is still polling, so we are polling too
            return Poll::Pending;
        };
        let Some(fut) = opt else {
            // If futures unordered returns None, that means there's 0 futures inside it.
            // So we should just return polling so the other "select" branches can run
            return Poll::Pending;
        };
        Poll::Ready(fut)
    }
}

#[derive(Debug)]
pub struct RecvBuffer {
    pub inner: [u8; RECV_BUF_SIZE],
    pub start: usize,
    // If usize::MAX, then empty
    pub end: usize,
}
impl Default for RecvBuffer {
    fn default() -> Self {
        Self::new()
    }
}
impl RecvBuffer {
    pub fn new() -> Self {
        Self {
            inner: [0; RECV_BUF_SIZE],
            start: 0,
            end: usize::MAX,
        }
    }
    pub fn len(&self) -> usize {
        if self.end == usize::MAX {
            return 0;
        }
        let len = (self.end + RECV_BUF_SIZE - self.start) % RECV_BUF_SIZE;
        if len == 0 {
            return RECV_BUF_SIZE;
        }
        len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn available(&self) -> usize {
        RECV_BUF_SIZE - self.len()
    }
}

pub struct MultiRecvConn {
    limiter: Arc<Limiter>,
    buf: RecvBuffer,
}

pub mod wordlist;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use futures::pin_mut;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::InMemoryState;
use governor::state::NotKeyed;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::net::TcpStream;

pub const RECV_BUF_SIZE: usize = 512 * 1024;
pub const MUX_RECV_BUF_SIZE: usize = 512 * 1024 + 1024;
pub const TIMEOUT_DURATION_SECS: f32 = 15.;
pub const SEND_AMT: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub enum Message<'a> {
    DomainDecided(&'a [u8]),
    Disconnect(u64),
    Connect(u64),
    Data(u64, &'a [u8]),
    HeartBeat,
    Acknowledged(u64),
}

#[derive(Debug)]
pub enum DecodeStatus {
    NeedMoreData,
    Error(std::io::Error),
}

trait ToDecodeFailure: Sized {
    type Output;
    fn or_need_more_data(self) -> Result<Self::Output, DecodeStatus>;
}

impl<T> ToDecodeFailure for Option<T> {
    type Output = T;
    fn or_need_more_data(self) -> Result<T, DecodeStatus> {
        match self {
            Some(v) => Ok(v),
            None => Err(DecodeStatus::NeedMoreData),
        }
    }
}

impl<'a> Message<'a> {
    fn get_discriminant(&self) -> u8 {
        match self {
            Message::DomainDecided(_) => 0,
            Message::Disconnect(_) => 1,
            Message::Connect(_) => 2,
            Message::Data(_, _) => 3,
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
    pub fn decode(buf: &'a [u8]) -> Result<(Message<'a>, usize), DecodeStatus> {
        let mut cursor = 0;
        let discriminant = buf.get(cursor).or_need_more_data()?;
        cursor += 1;
        match discriminant {
            0 => {
                let (len, read) = varint::decode_varint(&buf[cursor..])?;
                let len = len as usize;
                cursor += read;
                // Domain should not be >= 512 bytes
                if len >= 512 {
                    return Err(DecodeStatus::Error(std::io::ErrorKind::OutOfMemory.into()));
                }
                if let Some(slice) = buf.get(cursor..cursor + len) {
                    Ok((Message::DomainDecided(slice), cursor + len))
                } else {
                    Err(DecodeStatus::NeedMoreData)
                }
            }
            1 => {
                if let Some(n) = buf.get(cursor..cursor + 8) {
                    return Ok((
                        Message::Disconnect(u64::from_be_bytes(n.try_into().unwrap())),
                        cursor + 8,
                    ));
                }
                Err(DecodeStatus::NeedMoreData)
            }
            2 => {
                if let Some(n) = buf.get(cursor..cursor + 8) {
                    return Ok((
                        Message::Connect(u64::from_be_bytes(n.try_into().unwrap())),
                        cursor + 8,
                    ));
                }
                Err(DecodeStatus::NeedMoreData)
            }
            3 => {
                let (id, read) = varint::decode_varint(&buf[cursor..])?;
                cursor += read;
                let (len, read) = varint::decode_varint(&buf[cursor..])?;
                let len = len as usize;
                cursor += read;
                if len >= RECV_BUF_SIZE {
                    return Err(DecodeStatus::Error(std::io::ErrorKind::OutOfMemory.into()));
                }
                if let Some(slice) = buf.get(cursor..cursor + len) {
                    Ok((Message::Data(id, slice), cursor + len))
                } else {
                    Err(DecodeStatus::NeedMoreData)
                }
            }
            4 => Ok((Message::HeartBeat, 1)),
            5 => {
                if let Some(n) = buf.get(cursor..cursor + 8) {
                    return Ok((
                        Message::Acknowledged(u64::from_be_bytes(n.try_into().unwrap())),
                        cursor + 8,
                    ));
                }
                Err(DecodeStatus::NeedMoreData)
            }
            _ => Err(DecodeStatus::Error(std::io::ErrorKind::InvalidData.into())),
        }
    }
}

pub mod varint {

    use crate::DecodeStatus;

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

    pub fn decode_varint(buf: &[u8]) -> Result<(u64, usize), DecodeStatus> {
        use super::ToDecodeFailure;
        let mut result = 0;
        for i in 0..10 {
            let byte = *buf.get(i).or_need_more_data()?;
            result |= ((byte & 0x7f) as u64) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok((result, i + 1));
            }
        }
        Err(DecodeStatus::Error(std::io::ErrorKind::InvalidData.into()))
    }
}

pub type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[derive(Debug)]
pub struct SingleRecvConn {
    pub stream: TcpStream,
    pub limiter: Arc<Limiter>,
    pub buf: Box<[u8; 8096]>,
    pub buf_filled: Option<NonZeroU32>,
    pub id: u64,
}

impl SingleRecvConn {
    pub fn recv(&mut self) -> SingleConnReceiveDataFuture {
        SingleConnReceiveDataFuture { client: Some(self) }
    }
}

#[derive(Debug)]
pub struct SingleConnReceiveDataFuture<'a> {
    // Lifetime workaround
    client: Option<&'a mut SingleRecvConn>,
}

impl<'a> Future for SingleConnReceiveDataFuture<'a> {
    type Output = Result<usize, std::io::Error>;

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
                client.buf_filled = None;
                let n = n.get() as usize;
                Poll::Ready(Ok(n))
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
                                return Poll::Ready(Err(e));
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
                    return Poll::Ready(Ok(0));
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
                Poll::Ready(Ok(filled.get() as usize))
            }
        }
    }
}

#[derive(Debug)]
pub struct ArrayPollSingle<'a> {
    pub arr: Option<&'a mut [SingleRecvConn]>,
    pub index: Option<&'a mut usize>,
}

impl<'a> Future for ArrayPollSingle<'a> {
    type Output = (Result<usize, std::io::Error>, usize);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let arr = self.arr.take().unwrap();
        let index = self.index.take().unwrap();
        *index %= arr.len();
        for _ in 0..arr.len() {
            {
                let fut = arr[*index].recv();
                let fut = pin!(fut);
                match fut.poll(cx) {
                    Poll::Ready(res) => return Poll::Ready((res, *index)),
                    Poll::Pending => {}
                }
                *index += 1;
                *index %= arr.len();
            }
        }
        Poll::Pending
    }
}

#[derive(Debug)]
pub struct RecvBuffer {
    pub inner: [u8; RECV_BUF_SIZE],
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
            end: 0,
        }
    }
    pub fn len(&self) -> usize {
        self.end - 1
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn available(&self) -> usize {
        RECV_BUF_SIZE - self.len()
    }
    pub fn to_write(&self) -> usize {
        self.len().min(8096)
    }
    pub fn remaining(&self) -> usize {
        self.len() - self.to_write()
    }
    pub fn wrote(&mut self, bytes: usize) {
        let remaining = self.remaining();
        self.inner.copy_within(bytes..bytes + remaining, 0);
        self.end = remaining;
    }
    pub fn write(&mut self, slice: &[u8]) {
        self.inner[self.end..self.end + slice.len()].copy_from_slice(slice);
        self.end += slice.len();
    }
}

#[derive(Debug)]
pub struct RecvMux {
    pub limiter: Arc<Limiter>,
    pub buf: RecvBuffer,
    pub id: usize,
}

#[derive(Debug)]
pub struct RecvMuxFuture<'a> {
    // Lifetime workaround
    conn: Option<&'a mut RecvMux>,
}

impl<'a> Future for RecvMuxFuture<'a> {
    type Output = usize;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let conn = self.conn.take().unwrap();
        let len = conn.buf.len();
        if len == 0 {
            // if there's nothing to transfer, we don't have anything to do :)
            return Poll::Pending;
        }
        let to_write = conn.buf.to_write();
        let fut = conn
            .limiter
            // We just cast the len, because the max length of the buffer will NEVER be > 4gb.
            .until_n_ready(NonZeroU32::new(to_write as u32).unwrap());
        let fut = pin!(fut);
        match fut.poll(cx) {
            Poll::Ready(res) => {
                // Since we use small buffer this should always succeed
                debug_assert!(res.is_ok());
                Poll::Ready(conn.id)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
pub struct ArrayPollMux<'a> {
    pub arr: Option<&'a mut [RecvMux]>,
    pub index: Option<&'a mut usize>,
}

impl<'a> Future for ArrayPollMux<'a> {
    type Output = (usize, usize);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let arr = self.arr.take().unwrap();
        let index = self.index.take().unwrap();
        *index %= arr.len();
        for _ in 0..arr.len() {
            {
                let fut = RecvMuxFuture {
                    conn: Some(&mut arr[*index]),
                };
                let fut = pin!(fut);
                // blocked on https://github.com/rust-lang/rust/issues/54663
                match fut.poll(cx) {
                    Poll::Ready(res) => return Poll::Ready((res, *index)),
                    Poll::Pending => {}
                }
                *index += 1;
                *index %= arr.len();
            }
        }
        Poll::Pending
    }
}

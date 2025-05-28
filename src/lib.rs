mod wordlist;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

pub const DATA_BUF_SIZE: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub enum Message<'a> {
    DomainDecided(&'a [u8]),
    Disconnect(u64),
    Connect(u64),
    Data(&'a [u8]),
    HeartBeat,
}

impl<'a> Message<'a> {
    fn get_discriminant(&self) -> u8 {
        match self {
            Message::DomainDecided(_) => 0,
            Message::Disconnect(_) => 1,
            Message::Connect(_) => 2,
            Message::Data(_) => 3,
            Message::HeartBeat => 4,
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
            Message::Connect(id) | Message::Disconnect(id) => {
                writer.write_u64(id).await?;
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
                if len >= DATA_BUF_SIZE {
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

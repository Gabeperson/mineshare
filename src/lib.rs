#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]

use bincode::{BorrowDecode, Decode, Encode};
use std::io::ErrorKind;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::warn;

pub mod wordlist;

pub const PROTOCOL_VERSION: u64 = 1;

/// Messages transferred through the initial TLS stream between proxy and server
#[derive(Debug, Clone, Encode, Decode)]
pub enum Message {
    /// Proxy request to server to echo back these bytes
    HeartBeat([u8; 32]),
    /// Server response to heartbeat request, echoing back requested bytes
    HeartBeatEcho([u8; 32]),
    /// Request from proxy to server telling server to open the TCP stream that the client connection will
    /// be proxied through.
    NewClient(u128),
}

/// Initial message from proxy to server telling server the domain it is assigned, the proxy's public key,
/// and the proxy server's protocol number. This protocol number will be incremented every time the protocol changes.
#[derive(Debug, Clone, Encode, Decode)]
pub struct DomainAndPubKey {
    /// The url that was assigned to the Minecraft server. (ex: <word>-<word>-<word>.mineshare.dev)
    pub domain: String,
    /// The server's public key, used for authenticating the server during Diffie Hellman
    pub public_key: [u8; 32],
    /// Protocol version for the proxy server, not Minecraft protocol version
    pub protocol_version: u64,
}

impl DomainAndPubKey {
    #[must_use]
    pub fn new(domain: String, public_key: [u8; 32], protocol_version: u64) -> Self {
        Self {
            domain,
            public_key,
            protocol_version,
        }
    }
}

/// An IP Address + Port
#[derive(Debug, Clone, Encode, Decode)]
pub struct Addr(pub SocketAddr);

/// A simple "HELLO" string from server to proxy server to validate that the server is a valid mineshare server
/// and not a random port 443 connection.
/// It's probably not very smart to use 443 for something other than HTTPS, but it works so...
#[derive(Debug, Clone, Encode, BorrowDecode)]
pub struct ServerHello<'a>(pub &'a str);

impl<'a> BincodeAsync<'a> for ServerHello<'a> {}
impl BincodeAsync<'_> for Message {}
impl BincodeAsync<'_> for DomainAndPubKey {}
impl BincodeAsync<'_> for Addr {}

/// Helper trait for serializing & deserializing data into async streams
pub trait BincodeAsync<'a>: BorrowDecode<'a, ()> + Encode + Send {
    /// Encode this struct into the stream
    fn encode<S: AsyncWrite + Unpin + Send>(
        self,
        r: &mut S,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, std::io::Error>> + Send {
        async {
            let len = bincode::encode_into_slice(self, buf, bincode::config::standard())
                .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
            r.write_u32(len as u32).await?;
            r.write_all(&buf[..len]).await?;
            r.flush().await?;
            Ok(len + 4)
        }
    }
    /// Decode this struct from the stream
    fn parse<S: AsyncRead + Unpin + Send>(
        r: &mut S,
        buf: &'a mut [u8],
    ) -> impl Future<Output = Result<Self, std::io::Error>> + Send {
        async {
            let len = r.read_u32().await?;
            let len = len as usize;
            if len > buf.len() {
                return Err(ErrorKind::InvalidData.into());
            }
            let read = r.read_exact(&mut buf[..len]).await?;
            let (decoded, _len) = match bincode::borrow_decode_from_slice::<Self, _>(
                &buf[..read],
                bincode::config::standard(),
            ) {
                Ok(d) => d,
                Err(e) => return Err(std::io::Error::new(ErrorKind::InvalidData, e)),
            };
            Ok(decoded)
        }
    }
}

/// Try and parse the hostname from the initial client packet from a Minecraft client to a Minecraft server
/// Returns `Ok(Some(&[u8]))` if hostname was parsed successfully
/// Returns `Ok(None)` if it needs more data
/// Returns `Err(e)` if an error happened or data is malformed
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

/// Utilities for encoding and decoding varints
pub mod varint {
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

/// An encapsulation of a simple, secure authenticated Diffie Hellman with previous knowledge of a public key.
pub mod dhauth {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::BincodeAsync;
    use aes_gcm_siv::aead::Aead;
    use aes_gcm_siv::{Aes256GcmSiv, KeyInit as _, Nonce};
    use bincode::{BorrowDecode, Decode, Encode};
    use blake3::Hasher;
    use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
    use rand::Rng as _;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tracing::error;
    use x25519_dalek::{EphemeralSecret, PublicKey};

    #[derive(Clone, Debug, Encode, Decode)]
    struct BobPublic([u8; 32]);
    #[derive(Clone, Debug, Encode, Decode)]
    struct AlicePubSignature([u8; 32], [u8; 64]);
    #[derive(Clone, Debug, Encode, BorrowDecode)]
    struct Id<'a>([u8; 12], &'a [u8]);

    impl BincodeAsync<'_> for BobPublic {}
    impl BincodeAsync<'_> for AlicePubSignature {}
    impl<'a> BincodeAsync<'a> for Id<'a> {}

    pub struct AuthenticatorProxy<'a, S: AsyncRead + AsyncWrite + Send + Unpin> {
        pub inner: &'a mut S,
        pub alice_private_sign_key: SigningKey,
        pub counter: &'a AtomicU64,
    }

    impl<S: AsyncRead + AsyncWrite + Send + Unpin> AuthenticatorProxy<'_, S> {
        #[allow(clippy::missing_panics_doc)]
        pub async fn get_id(mut self) -> Result<u128, std::io::Error> {
            let mut buf = [0u8; 128];
            let alice_secret = EphemeralSecret::random();
            let alice_public = PublicKey::from(&alice_secret);

            let BobPublic(client_public) = BobPublic::parse(&mut self.inner, &mut buf).await?;
            let bob_public = PublicKey::from(client_public);

            let hashed = sha256(alice_public, bob_public);
            // Since we sign the hashed alice public and bob public, replay isn't possible because
            // The bob hash will be different in previous interactions
            // On-path MITM is also impossible because when bob verifies the signature, it will be off
            // and he will immediately terminate the connection, suspecting a MITM.
            let signature = self.alice_private_sign_key.sign(&hashed);
            let signature = signature.to_bytes();
            let len = AlicePubSignature(alice_public.to_bytes(), signature)
                .encode(&mut self.inner, &mut buf)
                .await?;
            self.counter.fetch_add(len as u64, Ordering::Relaxed);
            let Id(nonce, data) = Id::parse(&mut self.inner, &mut buf).await.map_err(|e| {
                std::io::Error::other(format!("Connection error OR A MITM OCCURED: {e}"))
            })?;

            // If we got here, it implicitly means that bob accepted our signature, which means
            // We weren't MITM'd.
            // Therefore, this should be the shared secret between bob and us
            let shared = alice_secret.diffie_hellman(&bob_public).to_bytes();
            let key = shared.into();
            let aes = Aes256GcmSiv::new(&key);

            let decrypted = aes
                .decrypt(&Nonce::from(nonce), data)
                .map_err(|_e| std::io::Error::other("Error occured when decrypting key"))?;

            if decrypted.len() != 16 {
                return Err(std::io::Error::other(format!(
                    "ID is {} bytes, expected 16",
                    decrypted.len()
                )));
            }

            let id = u128::from_be_bytes(decrypted[..].try_into().unwrap());

            Ok(id)
        }
    }
    pub struct AuthenticatorServer<'a, S: AsyncRead + AsyncWrite + Send + Unpin> {
        pub inner: &'a mut S,
        pub alice_public_sign_key: VerifyingKey,
    }
    impl<S: AsyncRead + AsyncWrite + Send + Unpin> AuthenticatorServer<'_, S> {
        pub async fn send_id(mut self, id: u128) -> Result<(), std::io::Error> {
            let mut buf = [0u8; 1024];
            let bob_secret = EphemeralSecret::random();
            let bob_public = PublicKey::from(&bob_secret);

            BobPublic(bob_public.to_bytes())
                .encode(&mut self.inner, &mut buf)
                .await?;
            let AlicePubSignature(alice_public, signature) =
                AlicePubSignature::parse(&mut self.inner, &mut buf).await?;
            let alice_public = PublicKey::from(alice_public);

            // Since the alice and bob public keys should be the same on alice's side as ours,
            // the hash should also be the same
            let hashed = sha256(alice_public, bob_public);

            // If the hash is the same, then the verification should pass. Otherwise,
            // it's probably a MITM.
            let res = self
                .alice_public_sign_key
                .verify(&hashed, &Signature::from(signature));
            if let Err(e) = res {
                error!("POTENTIAL MITM? Signature error!!! {e}");
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
            // Since the hash matched, this should be the shared secret between alice and us
            let shared = bob_secret.diffie_hellman(&alice_public).to_bytes();
            let key = shared.into();
            let aes = Aes256GcmSiv::new(&key);
            let mut nonce = [0u8; 12];
            rand::rng().fill(&mut nonce);
            let nonce_gen = Nonce::from(nonce);
            let id = id.to_be_bytes();
            let ciphertext = aes
                .encrypt(&nonce_gen, &id as &[u8])
                .map_err(|_e| std::io::Error::other("An error occured while encrypting id"))?;

            Id(nonce, &ciphertext)
                .encode(&mut self.inner, &mut buf)
                .await?;

            // We've sent the ID, so now we're done :)
            Ok(())
        }
    }
    fn sha256(pub1: PublicKey, pub2: PublicKey) -> [u8; 32] {
        let mut hasher = Hasher::new();
        hasher.update(b"MINESHARE");
        hasher.update(pub1.as_bytes());
        hasher.update(pub2.as_bytes());
        hasher.finalize().into()
    }
}

// SPDX-License-Identifier: MPL-2.0
//
// Ported from whatsmeow (https://github.com/tulir/whatsmeow, c88388f), socket/noisehandshake.go,
// socket/noisesocket.go, socket/framesocket.go and handshake.go, Copyright (c) Tulir Asokan,
// MPL-2.0. Modified for whatsapp-sync: no I/O (the caller owns the websocket), the `ED` routing
// header Baileys sends, and the certificate root and clock passed in so they can be tested.

//! WhatsApp's encrypted transport: a Noise XX handshake, then AES-GCM frames.
//!
//! WhatsApp's Noise is the standard pattern with two local quirks, both matched here and both
//! pinned by `tests/noise.rs` against WAHA's own Baileys: the empty payload of the first message
//! is not mixed into the hash (the spec mixes it), and the server proves who it is with a
//! certificate chain to a fixed WhatsApp root key rather than a pinned static key.
//!
//! Everything is byte-in, byte-out. [`Handshake`] makes the two client messages; [`Transport`]
//! seals and opens frames after it; [`Frames`] adds and strips the 3-byte length prefix.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use hkdf::Hkdf;
use prost::Message;
use sha2::{Digest, Sha256};

use crate::binary::tokens::DICT_VERSION;
use crate::proto::{CertChain, CertDetails, ClientFinish, HandshakeHello, HandshakeMessage};
use crate::signal::curve::{KeyError, KeyPair, PublicKey};

pub const URL: &str = "wss://web.whatsapp.com/ws/chat";
pub const ORIGIN: &str = "https://web.whatsapp.com";

/// Exactly 32 bytes, so it is the initial hash itself rather than hashed.
const PATTERN: &[u8; 32] = b"Noise_XX_25519_AESGCM_SHA256\0\0\0\0";
/// `WA`, the protocol magic, the token dictionary version: the handshake's prologue.
pub const WA_HEADER: [u8; 4] = [b'W', b'A', 6, DICT_VERSION];

/// WhatsApp's long-term Noise certificate root ("WhatsAppLongTerm1"). Every server's static key
/// must chain to it through an intermediate whose issuer serial is 0.
pub const WA_CERT_ROOT: [u8; 32] = [
    0x14, 0x23, 0x75, 0x57, 0x4d, 0x0a, 0x58, 0x71, 0x66, 0xaa, 0xe7, 0x1e, 0xbe, 0x51, 0x64, 0x37,
    0xc4, 0xa2, 0x8b, 0x73, 0xe3, 0x69, 0x5c, 0x6c, 0xe1, 0xf7, 0xf9, 0x54, 0x5d, 0xa8, 0xee, 0x6b,
];
const WA_CERT_ISSUER_SERIAL: u32 = 0;

/// Frames carry a 3-byte length.
pub const MAX_FRAME: usize = (1 << 24) - 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoiseError {
    #[error("server hello is malformed or incomplete")]
    BadServerHello,
    #[error("a handshake ciphertext does not authenticate")]
    Decrypt,
    #[error("server static key is not 32 bytes")]
    BadStatic,
    #[error("server certificate: {0}")]
    Certificate(&'static str),
    #[error("frame of {0} bytes is over the 16 MiB limit")]
    FrameTooLarge(usize),
    #[error(transparent)]
    Key(#[from] KeyError),
}

/// The running hash, chaining key and cipher both sides keep through the handshake.
struct Symmetric {
    hash: [u8; 32],
    salt: [u8; 32],
    key: Aes256Gcm,
    counter: u32,
}

impl Symmetric {
    fn new() -> Self {
        Self {
            hash: *PATTERN,
            salt: *PATTERN,
            key: cipher(PATTERN),
            counter: 0,
        }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.hash = Sha256::new()
            .chain_update(self.hash)
            .chain_update(data)
            .finalize()
            .into();
    }

    fn mix_key(&mut self, input: &[u8]) {
        let (write, read) = expand(&self.salt, input);
        self.salt = write;
        self.key = cipher(&read);
        self.counter = 0;
    }

    fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ciphertext = self
            .key
            .encrypt(
                &iv(self.counter).into(),
                Payload {
                    msg: plaintext,
                    aad: &self.hash,
                },
            )
            .expect("AES-GCM sealing does not fail for these lengths");
        self.counter += 1;
        self.mix_hash(&ciphertext);
        ciphertext
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let plaintext = self
            .key
            .decrypt(
                &iv(self.counter).into(),
                Payload {
                    msg: ciphertext,
                    aad: &self.hash,
                },
            )
            .map_err(|_| NoiseError::Decrypt)?;
        self.counter += 1;
        self.mix_hash(ciphertext);
        Ok(plaintext)
    }
}

pub struct Handshake {
    state: Symmetric,
    ephemeral: KeyPair,
}

impl Handshake {
    /// Starts a handshake with a fresh ephemeral key.
    pub fn new() -> Self {
        Self::with_ephemeral(KeyPair::generate())
    }

    pub fn with_ephemeral(ephemeral: KeyPair) -> Self {
        let mut state = Symmetric::new();
        state.mix_hash(&WA_HEADER);
        state.mix_hash(ephemeral.public.bytes());
        Self { state, ephemeral }
    }

    /// The first message: our ephemeral key.
    pub fn client_hello(&self) -> Vec<u8> {
        HandshakeMessage {
            client_hello: Some(HandshakeHello {
                ephemeral: Some(self.ephemeral.public.bytes().to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// Reads the server hello, checks the server's certificate against `root` at `now` (Unix
    /// seconds), and answers with our static noise key and the login `payload`. Returns that
    /// answer and the transport the session continues on.
    pub fn client_finish(
        mut self,
        server_hello: &[u8],
        noise_key: &KeyPair,
        payload: &[u8],
        root: &[u8; 32],
        now: u64,
    ) -> Result<(Vec<u8>, Transport), NoiseError> {
        let hello = HandshakeMessage::decode(server_hello)
            .ok()
            .and_then(|m| m.server_hello)
            .ok_or(NoiseError::BadServerHello)?;
        let (Some(ephemeral), Some(static_ciphertext), Some(cert_ciphertext)) =
            (hello.ephemeral, hello.r#static, hello.payload)
        else {
            return Err(NoiseError::BadServerHello);
        };
        let server_ephemeral =
            PublicKey::from_djb(&ephemeral).map_err(|_| NoiseError::BadServerHello)?;
        let state = &mut self.state;
        state.mix_hash(server_ephemeral.bytes());
        state.mix_key(&self.ephemeral.private.agreement(&server_ephemeral)?);
        let server_static = PublicKey::from_djb(&state.decrypt(&static_ciphertext)?)
            .map_err(|_| NoiseError::BadStatic)?;
        state.mix_key(&self.ephemeral.private.agreement(&server_static)?);
        verify_certificate(&state.decrypt(&cert_ciphertext)?, &server_static, root, now)?;

        let static_ciphertext = state.encrypt(noise_key.public.bytes());
        state.mix_key(&noise_key.private.agreement(&server_ephemeral)?);
        let payload_ciphertext = state.encrypt(payload);
        let finish = HandshakeMessage {
            client_finish: Some(ClientFinish {
                r#static: Some(static_ciphertext),
                payload: Some(payload_ciphertext),
            }),
            ..Default::default()
        }
        .encode_to_vec();
        let (write, read) = expand(&state.salt, &[]);
        Ok((
            finish,
            Transport {
                write: cipher(&write),
                read: cipher(&read),
                write_counter: 0,
                read_counter: 0,
            },
        ))
    }
}

impl Default for Handshake {
    fn default() -> Self {
        Self::new()
    }
}

/// The intermediate must be signed by `root` with issuer serial 0, the leaf by the intermediate,
/// the leaf must name the static key the server just proved it holds, and both must be current.
fn verify_certificate(
    chain: &[u8],
    server_static: &PublicKey,
    root: &[u8; 32],
    now: u64,
) -> Result<(), NoiseError> {
    let fail = NoiseError::Certificate;
    let chain = CertChain::decode(chain).map_err(|_| fail("does not decode"))?;
    let (Some(intermediate), Some(leaf)) = (chain.intermediate, chain.leaf) else {
        return Err(fail("missing intermediate or leaf"));
    };
    let (Some(inter_details), Some(inter_signature), Some(leaf_details), Some(leaf_signature)) = (
        intermediate.details,
        intermediate.signature,
        leaf.details,
        leaf.signature,
    ) else {
        return Err(fail("missing details or signature"));
    };
    if !PublicKey::from_djb(root)?.verify_signature(&inter_details, &inter_signature) {
        return Err(fail("intermediate is not signed by the root"));
    }
    let inter =
        CertDetails::decode(inter_details.as_slice()).map_err(|_| fail("intermediate details"))?;
    if inter.issuer_serial.unwrap_or(0) != WA_CERT_ISSUER_SERIAL {
        return Err(fail("intermediate has the wrong issuer serial"));
    }
    let inter_key = PublicKey::from_djb(inter.key.as_deref().unwrap_or_default())
        .map_err(|_| fail("intermediate key"))?;
    if !inter_key.verify_signature(&leaf_details, &leaf_signature) {
        return Err(fail("leaf is not signed by the intermediate"));
    }
    current(&inter, now)?;
    let leaf = CertDetails::decode(leaf_details.as_slice()).map_err(|_| fail("leaf details"))?;
    if leaf.issuer_serial.unwrap_or(0) != inter.serial.unwrap_or(0) {
        return Err(fail("leaf was not issued by this intermediate"));
    }
    if leaf.key.as_deref() != Some(server_static.bytes().as_slice()) {
        return Err(fail("leaf names a different key than the server holds"));
    }
    current(&leaf, now)
}

fn current(details: &CertDetails, now: u64) -> Result<(), NoiseError> {
    if now < details.not_before.unwrap_or(0) {
        return Err(NoiseError::Certificate("not valid yet"));
    }
    if now > details.not_after.unwrap_or(0) {
        return Err(NoiseError::Certificate("expired"));
    }
    Ok(())
}

/// The session after the handshake: one key and counter each way.
pub struct Transport {
    write: Aes256Gcm,
    read: Aes256Gcm,
    write_counter: u32,
    read_counter: u32,
}

impl Transport {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ciphertext = self
            .write
            .encrypt(&iv(self.write_counter).into(), plaintext)
            .expect("AES-GCM sealing does not fail");
        self.write_counter += 1;
        ciphertext
    }

    /// The counter moves even on failure, as both whatsmeow and Baileys do: the server's
    /// counter already has, so staying put would fail every frame after a bad one.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let counter = self.read_counter;
        self.read_counter += 1;
        self.read
            .decrypt(&iv(counter).into(), ciphertext)
            .map_err(|_| NoiseError::Decrypt)
    }
}

/// Length-prefixed framing on the websocket. The first frame out also carries the prologue:
/// `WA` alone, or behind Baileys' `ED` routing header when the session has routing info.
pub struct Frames {
    intro: Option<Vec<u8>>,
    inbox: Vec<u8>,
}

impl Frames {
    pub fn new(routing_info: Option<&[u8]>) -> Self {
        let intro = match routing_info {
            None => WA_HEADER.to_vec(),
            Some(info) => {
                let len = info.len();
                let mut intro = vec![
                    b'E',
                    b'D',
                    0,
                    1,
                    (len >> 16) as u8,
                    (len >> 8) as u8,
                    len as u8,
                ];
                intro.extend_from_slice(info);
                intro.extend_from_slice(&WA_HEADER);
                intro
            }
        };
        Self {
            intro: Some(intro),
            inbox: Vec::new(),
        }
    }

    pub fn encode(&mut self, data: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let len = data.len();
        if len > MAX_FRAME {
            return Err(NoiseError::FrameTooLarge(len));
        }
        let mut out = self.intro.take().unwrap_or_default();
        out.extend([(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        out.extend_from_slice(data);
        Ok(out)
    }

    /// Takes whatever the websocket delivered: part of a frame, one, or several.
    pub fn push(&mut self, bytes: &[u8]) {
        self.inbox.extend_from_slice(bytes);
    }

    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let header: [u8; 3] = self.inbox.get(..3)?.try_into().ok()?;
        let len =
            usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
        if self.inbox.len() < 3 + len {
            return None;
        }
        let frame = self.inbox[3..3 + len].to_vec();
        self.inbox.drain(..3 + len);
        Some(frame)
    }
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("32-byte key")
}

/// 96-bit nonce: eight zero bytes and the counter, big-endian.
fn iv(counter: u32) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[8..].copy_from_slice(&counter.to_be_bytes());
    iv
}

/// HKDF-SHA256 with the chaining key as salt and no info, split into two 32-byte keys.
fn expand(salt: &[u8; 32], input: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(Some(salt), input)
        .expand(&[], &mut okm)
        .expect("64 bytes is a valid HKDF length");
    let (write, read) = okm.split_at(32);
    (
        write.try_into().expect("32 bytes"),
        read.try_into().expect("32 bytes"),
    )
}

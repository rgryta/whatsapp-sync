//! Voice notes: fetching one from WhatsApp's CDN and decrypting it.
//!
//! A media message carries no media, only where to fetch it and how to open it: a CDN path,
//! a 32-byte media key, and hashes. The key expands (HKDF, "WhatsApp Audio Keys") into an IV,
//! an AES-256-CBC key and an HMAC key; the file is the ciphertext followed by the first 10
//! bytes of HMAC-SHA256(IV ‖ ciphertext). This is Baileys' `getMediaKeys` and
//! `downloadEncryptedContent`; `tests/media.rs` decrypts what Baileys' `encryptedStream` made.
//!
//! The reference is kept in the archive row's `raw`, so a voice note can be fetched later, by a
//! worker, as many times as transcription needs. WAHA's rows keep the same fields in their own
//! shape, and [`MediaRef::from_raw`] reads both.

use aes::cipher::{KeyIvInit, block_padding::Pkcs7};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cbc::cipher::BlockModeDecrypt;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Baileys' `DEF_MEDIA_HOST`.
pub const MEDIA_BASE: &str = "https://mmg.whatsapp.net";
const MAC_LENGTH: usize = 10;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("the media key is not 32 bytes")]
    BadKey,
    #[error("the file's hash is not the one the message names")]
    EncryptedHash,
    #[error("the file's MAC does not verify")]
    Mac,
    #[error("the file does not decrypt")]
    Decrypt,
    #[error("the decrypted file's hash is not the one the message names")]
    PlainHash,
    #[error("the message names no place to fetch it from")]
    NoLocation,
}

/// Where a media file is and how to open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRef {
    pub direct_path: Option<String>,
    pub url: Option<String>,
    pub media_key: Vec<u8>,
    pub file_enc_sha256: Option<Vec<u8>>,
    pub file_sha256: Option<Vec<u8>>,
}

impl MediaRef {
    /// The form kept in this client's `raw`.
    pub fn to_json(&self) -> Value {
        json!({
            "directPath": self.direct_path,
            "url": self.url,
            "mediaKey": B64.encode(&self.media_key),
            "fileEncSha256": self.file_enc_sha256.as_ref().map(|b| B64.encode(b)),
            "fileSha256": self.file_sha256.as_ref().map(|b| B64.encode(b)),
        })
    }

    /// From an archive row's `raw`: this client's `media`, or WAHA's
    /// `_data.message.audioMessage`, whose bytes are Buffer objects.
    pub fn from_raw(raw: &Value) -> Option<Self> {
        let node = raw
            .get("media")
            .or_else(|| raw.pointer("/_data/message/audioMessage"))?;
        let bytes = |key: &str| bytes_of(node.get(key)?);
        Some(Self {
            direct_path: node
                .get("directPath")
                .and_then(Value::as_str)
                .map(str::to_owned),
            url: node.get("url").and_then(Value::as_str).map(str::to_owned),
            media_key: bytes("mediaKey")?,
            file_enc_sha256: bytes("fileEncSha256"),
            file_sha256: bytes("fileSha256"),
        })
    }

    /// Baileys' `getUrlFromDirectPath`, falling back to the message's own URL.
    pub fn location(&self, base: &str) -> Result<String, MediaError> {
        match (&self.direct_path, &self.url) {
            (Some(path), _) => Ok(format!("{base}{path}")),
            (None, Some(url)) => Ok(url.clone()),
            (None, None) => Err(MediaError::NoLocation),
        }
    }
}

/// A base64 string, or a Node Buffer as JSON: `{"type": "Buffer", "data": <base64 | [bytes]>}`.
pub(crate) fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(s) => B64.decode(s).ok(),
        Value::Object(o) => match o.get("data")? {
            Value::String(s) => B64.decode(s).ok(),
            Value::Array(items) => items
                .iter()
                .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect(),
            _ => None,
        },
        _ => None,
    }
}

/// Opens a downloaded voice note. Every check Baileys makes, before any byte is trusted.
pub fn decrypt_audio(file: &[u8], media: &MediaRef) -> Result<Vec<u8>, MediaError> {
    let key: [u8; 32] = media
        .media_key
        .as_slice()
        .try_into()
        .map_err(|_| MediaError::BadKey)?;
    if let Some(expected) = &media.file_enc_sha256
        && Sha256::digest(file).as_slice() != expected.as_slice()
    {
        return Err(MediaError::EncryptedHash);
    }
    let mut expanded = [0u8; 112];
    Hkdf::<Sha256>::new(None, &key)
        .expand(b"WhatsApp Audio Keys", &mut expanded)
        .expect("112 bytes is a valid HKDF length");
    let (iv, rest) = expanded.split_at(16);
    let (cipher_key, rest) = rest.split_at(32);
    let mac_key = &rest[..32];
    let body_len = file.len().checked_sub(MAC_LENGTH).ok_or(MediaError::Mac)?;
    let (ciphertext, mac) = file.split_at(body_len);
    let mut hmac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(mac_key).expect("HMAC takes any key length");
    hmac.update(iv);
    hmac.update(ciphertext);
    if !bool::from(hmac.finalize().into_bytes()[..MAC_LENGTH].ct_eq(mac)) {
        return Err(MediaError::Mac);
    }
    let plain = cbc::Decryptor::<aes::Aes256>::new_from_slices(cipher_key, iv)
        .expect("key and IV lengths are fixed")
        .decrypt_padded_vec::<Pkcs7>(ciphertext)
        .map_err(|_| MediaError::Decrypt)?;
    if let Some(expected) = &media.file_sha256
        && Sha256::digest(&plain).as_slice() != expected.as_slice()
    {
        return Err(MediaError::PlainHash);
    }
    Ok(plain)
}

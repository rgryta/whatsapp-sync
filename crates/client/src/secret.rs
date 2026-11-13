//! Edits encrypted under the edited message's own secret: how current phones send them.
//!
//! Every message carries a random `messageSecret`. An edit arrives as a `secretEncryptedMessage`
//! naming the original's key; its payload is AES-256-GCM under
//! HKDF-SHA256(secret, info = original id ‖ original sender ‖ editor ‖ "Message Edit"), with no
//! associated data. Ported from whatsmeow's `msgsecret.go` (`generateMsgSecretKey`,
//! `decryptMsgSecret`). Baileys, and so WAHA, never decrypted these, which is why the archive
//! held no edits before this client.
//!
//! The senders are bare accounts (`user@server`, no device), and which form, phone number or
//! LID, depends on the sender's migration: whatsmeow tries two and so does [`open`], over a few.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hkdf::Hkdf;
use serde_json::Value;
use sha2::Sha256;

use crate::binary::Jid;
use crate::media::bytes_of;

const MESSAGE_EDIT: &str = "Message Edit";

/// A JID as an account: `user@server`, the device dropped.
pub fn account(jid: &Jid) -> String {
    format!("{}@{}", jid.user, jid.server)
}

/// The plaintext of one edit, if the key derived for this pair of senders opens it.
pub fn decrypt(
    secret: &[u8],
    original_id: &str,
    original_sender: &str,
    editor: &str,
    iv: &[u8],
    payload: &[u8],
) -> Option<Vec<u8>> {
    let info = [
        original_id.as_bytes(),
        original_sender.as_bytes(),
        editor.as_bytes(),
        MESSAGE_EDIT.as_bytes(),
    ]
    .concat();
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(None, secret)
        .expand(&info, &mut key)
        .expect("32 bytes is a valid HKDF length");
    let iv: [u8; 12] = iv.try_into().ok()?;
    Aes256Gcm::new_from_slice(&key)
        .expect("32-byte key")
        .decrypt(&iv.into(), payload)
        .ok()
}

/// Tries every pairing of the candidate senders; GCM's tag rejects every wrong one.
pub fn open(
    secret: &[u8],
    original_id: &str,
    original_senders: &[String],
    editors: &[String],
    iv: &[u8],
    payload: &[u8],
) -> Option<Vec<u8>> {
    original_senders.iter().find_map(|original| {
        editors
            .iter()
            .find_map(|editor| decrypt(secret, original_id, original, editor, iv, payload))
    })
}

/// The secret an archived message kept in `raw`: this client's `messageSecret`, or WAHA's
/// `_data.message.messageContextInfo.messageSecret` (a Node Buffer as JSON).
pub fn from_raw(raw: &Value) -> Option<Vec<u8>> {
    match raw.get("messageSecret") {
        Some(Value::String(s)) => B64.decode(s).ok(),
        _ => bytes_of(raw.pointer("/_data/message/messageContextInfo/messageSecret")?),
    }
}

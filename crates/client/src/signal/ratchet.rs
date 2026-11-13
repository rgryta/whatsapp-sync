//
// Copyright 2020-2025 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//
// From libsignal v0.74.1, rust/protocol/src/ratchet.rs and ratchet/keys.rs. Modified for
// whatsapp-sync: version 3 (X3DH) only, so no Kyber or post-quantum ratchet; only the
// responder ("Bob") side, since this client never starts a session; and the base-key check of
// libsignal 5293caa6c (2025-09) applied.

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::SignalError;
use super::curve::{KeyPair, PrivateKey, PublicKey};
use super::session::SessionState;

#[derive(Clone)]
pub(super) struct ChainKey {
    pub key: [u8; 32],
    pub index: u32,
}

impl ChainKey {
    pub fn next(&self) -> Self {
        Self {
            key: hmac_sha256(&self.key, &[0x02]),
            index: self.index + 1,
        }
    }

    /// The seed this chain position's message keys expand from.
    pub fn message_key_seed(&self) -> [u8; 32] {
        hmac_sha256(&self.key, &[0x01])
    }
}

pub(super) struct MessageKeys {
    pub cipher_key: [u8; 32],
    pub mac_key: [u8; 32],
    pub iv: [u8; 16],
}

impl MessageKeys {
    pub fn derive(seed: &[u8]) -> Self {
        let mut okm = [0u8; 80];
        Hkdf::<Sha256>::new(None, seed)
            .expand(b"WhisperMessageKeys", &mut okm)
            .expect("80 bytes is a valid HKDF length");
        let mut keys = Self {
            cipher_key: [0; 32],
            mac_key: [0; 32],
            iv: [0; 16],
        };
        keys.cipher_key.copy_from_slice(&okm[..32]);
        keys.mac_key.copy_from_slice(&okm[32..64]);
        keys.iv.copy_from_slice(&okm[64..]);
        keys
    }
}

pub(super) struct RootKey(pub [u8; 32]);

impl RootKey {
    /// One step of the Diffie-Hellman ratchet: a new root key and a chain starting at 0.
    pub fn create_chain(
        &self,
        theirs: &PublicKey,
        ours: &PrivateKey,
    ) -> Result<(RootKey, ChainKey), SignalError> {
        let shared = ours.agreement(theirs)?;
        let mut okm = [0u8; 64];
        Hkdf::<Sha256>::new(Some(&self.0), &shared)
            .expand(b"WhisperRatchet", &mut okm)
            .expect("64 bytes is a valid HKDF length");
        let (root, chain) = okm.split_at(32);
        Ok((
            RootKey(root.try_into().expect("32 bytes")),
            ChainKey {
                key: chain.try_into().expect("32 bytes"),
                index: 0,
            },
        ))
    }
}

pub(super) struct BobParameters<'a> {
    pub our_identity: &'a KeyPair,
    pub our_signed_pre_key: &'a KeyPair,
    pub our_one_time_pre_key: Option<&'a KeyPair>,
    pub their_identity: &'a PublicKey,
    pub their_base_key: &'a PublicKey,
}

/// X3DH from the receiving side: the session a contact's first message opens.
pub(super) fn initialize_bob_session(p: &BobParameters) -> Result<SessionState, SignalError> {
    if !p.their_base_key.is_canonical() {
        return Err(SignalError::InvalidMessage("incoming base key is invalid"));
    }
    let mut secrets = Vec::with_capacity(32 * 5);
    secrets.extend_from_slice(&[0xFF; 32]); // "discontinuity bytes"
    secrets.extend_from_slice(&p.our_signed_pre_key.private.agreement(p.their_identity)?);
    secrets.extend_from_slice(&p.our_identity.private.agreement(p.their_base_key)?);
    secrets.extend_from_slice(&p.our_signed_pre_key.private.agreement(p.their_base_key)?);
    if let Some(one_time) = p.our_one_time_pre_key {
        secrets.extend_from_slice(&one_time.private.agreement(p.their_base_key)?);
    }
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(None, &secrets)
        .expand(b"WhisperText", &mut okm)
        .expect("64 bytes is a valid HKDF length");
    let (root, chain) = okm.split_at(32);
    let mut session = SessionState::new(
        &p.our_identity.public,
        p.their_identity,
        &RootKey(root.try_into().expect("32 bytes")),
        p.their_base_key,
    );
    // The responder's first ratchet key is its signed pre-key.
    session.set_sender_chain(
        p.our_signed_pre_key,
        &ChainKey {
            key: chain.try_into().expect("32 bytes"),
            index: 0,
        },
    );
    Ok(session)
}

pub(super) fn hmac_sha256(key: &[u8], input: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(input);
    mac.finalize().into_bytes().into()
}

//
// Copyright 2020-2025 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//
// The decrypt path from libsignal v0.74.1, rust/protocol/src/session_cipher.rs, session.rs and
// protocol.rs. Modified for whatsapp-sync: receive-only, version 3 (X3DH) only, synchronous
// in-memory store, identities trusted on first use and replaced on change as WhatsApp Web does.

//! Signal's pairwise protocol as WhatsApp speaks it: version 3, X3DH plus the double ratchet.
//!
//! Signal removed version 3 from libsignal after v0.74.1, so this is that release's code,
//! cut down to what a receive-only device runs and kept here. Only the protocol logic is
//! frozen; the primitives under it (curve25519-dalek, RustCrypto's AES, HMAC and HKDF) are
//! ordinary dependencies and update as they do. See `curve.rs` and `ratchet.rs` for the two
//! later upstream hardening fixes carried over.

pub mod curve;
mod ratchet;
mod session;

use std::collections::HashMap;

use aes::cipher::{KeyIvInit, block_padding::Pkcs7};
use cbc::cipher::BlockModeDecrypt;
use hmac::{Hmac, KeyInit, Mac};
use prost::Message;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use self::curve::{KeyError, KeyPair, PublicKey};
use self::ratchet::{BobParameters, ChainKey, MessageKeys};
pub use self::session::SessionRecord;
use self::session::SessionState;
use crate::proto::{WirePreKeySignalMessage, WireSignalMessage};

/// How far ahead of its chain a message may be.
const MAX_FORWARD_JUMPS: u32 = 25_000;
const MAC_LENGTH: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignalError {
    #[error("not a version-3 message (version {0})")]
    Version(u8),
    #[error("invalid message: {0}")]
    InvalidMessage(&'static str),
    #[error("message {counter} was already decrypted")]
    Duplicate { counter: u32 },
    #[error("no session with this device")]
    NoSession,
    #[error("unknown signed pre-key {0}")]
    MissingSignedPreKey(u32),
    #[error("unknown one-time pre-key {0}")]
    MissingPreKey(u32),
    #[error("stored session is unusable: {0}")]
    InvalidState(&'static str),
    #[error(transparent)]
    Key(#[from] KeyError),
}

/// A contact device as Signal addresses it: `user[_domain]` and a device number.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Address {
    pub name: String,
    pub device: u32,
}

/// Everything decryption reads and writes. Every operation changes it only when the
/// message decrypts, so a forged or broken message leaves no trace.
pub struct Store {
    pub identity: KeyPair,
    pub registration_id: u32,
    pub signed_pre_keys: HashMap<u32, KeyPair>,
    pub pre_keys: HashMap<u32, KeyPair>,
    pub sessions: HashMap<Address, SessionRecord>,
    pub identities: HashMap<Address, PublicKey>,
    /// One-time pre-keys consumed since the caller last drained this, so it can delete them
    /// from durable storage too: a pre-key must never serve two sessions.
    pub used_pre_keys: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Whisper,
    PreKey,
}

impl Store {
    /// A `pkmsg`: a message that may open a session.
    pub fn decrypt_prekey(
        &mut self,
        from: &Address,
        serialized: &[u8],
    ) -> Result<Vec<u8>, SignalError> {
        let message = PreKeySignalMessage::parse(serialized)?;
        let mut record = self.sessions.get(from).cloned().unwrap_or_default();
        let used_pre_key = self.process_prekey(&message, &mut record)?;
        let plaintext = decrypt_with_record(&mut record, &message.message, Kind::PreKey)?;
        self.identities.insert(from.clone(), message.identity_key);
        self.sessions.insert(from.clone(), record);
        if let Some(id) = used_pre_key {
            self.pre_keys.remove(&id);
            self.used_pre_keys.push(id);
        }
        Ok(plaintext)
    }

    /// A `msg`: a message on a session that already exists.
    pub fn decrypt(&mut self, from: &Address, serialized: &[u8]) -> Result<Vec<u8>, SignalError> {
        let message = SignalMessage::parse(serialized)?;
        let mut record = self
            .sessions
            .get(from)
            .cloned()
            .ok_or(SignalError::NoSession)?;
        let plaintext = decrypt_with_record(&mut record, &message, Kind::Whisper)?;
        let current = record
            .current
            .as_ref()
            .expect("a decrypted message leaves a current session");
        self.identities
            .insert(from.clone(), current.remote_identity()?);
        self.sessions.insert(from.clone(), record);
        Ok(plaintext)
    }

    /// Returns the one-time pre-key the message used up, if it opened a new session.
    fn process_prekey(
        &self,
        message: &PreKeySignalMessage,
        record: &mut SessionRecord,
    ) -> Result<Option<u32>, SignalError> {
        if record.promote_matching_session(&message.base_key.serialize())? {
            return Ok(None);
        }
        let signed = self
            .signed_pre_keys
            .get(&message.signed_pre_key_id)
            .ok_or(SignalError::MissingSignedPreKey(message.signed_pre_key_id))?;
        let one_time = message
            .pre_key_id
            .map(|id| self.pre_keys.get(&id).ok_or(SignalError::MissingPreKey(id)))
            .transpose()?;
        let mut state = ratchet::initialize_bob_session(&BobParameters {
            our_identity: &self.identity,
            our_signed_pre_key: signed,
            our_one_time_pre_key: one_time,
            their_identity: &message.identity_key,
            their_base_key: &message.base_key,
        })?;
        state.set_registration_ids(self.registration_id, message.registration_id);
        record.promote_state(state);
        Ok(message.pre_key_id)
    }
}

/// Tries the current session, then each archived one. The first that decrypts becomes current.
fn decrypt_with_record(
    record: &mut SessionRecord,
    message: &SignalMessage,
    kind: Kind,
) -> Result<Vec<u8>, SignalError> {
    if let Some(current) = &record.current {
        let mut state = current.clone();
        match decrypt_with_state(&mut state, message) {
            Ok(plaintext) => {
                record.current = Some(state);
                return Ok(plaintext);
            }
            Err(e @ SignalError::Duplicate { .. }) => return Err(e),
            // A pre-key message names its session; if that one fails, no other may answer.
            Err(_) if kind == Kind::PreKey => {
                return Err(SignalError::InvalidMessage("decryption failed"));
            }
            Err(_) => {}
        }
    }
    for index in 0..record.previous.len() {
        let mut state = record.previous_state(index)?;
        match decrypt_with_state(&mut state, message) {
            Ok(plaintext) => {
                record.promote_old_session(index, state);
                return Ok(plaintext);
            }
            Err(e @ SignalError::Duplicate { .. }) => return Err(e),
            Err(_) => {}
        }
    }
    if record.current.is_none() && record.previous.is_empty() {
        return Err(SignalError::NoSession);
    }
    Err(SignalError::InvalidMessage("decryption failed"))
}

fn decrypt_with_state(
    state: &mut SessionState,
    message: &SignalMessage,
) -> Result<Vec<u8>, SignalError> {
    if u32::from(message.version) != state.session_version() {
        return Err(SignalError::Version(message.version));
    }
    let chain_key = chain_key_for(state, &message.ratchet_key)?;
    let seed = message_key_seed(state, &message.ratchet_key, &chain_key, message.counter)?;
    let keys = MessageKeys::derive(&seed);
    if !message.verify_mac(
        &state.remote_identity()?,
        &state.local_identity()?,
        &keys.mac_key,
    ) {
        return Err(SignalError::InvalidMessage("MAC verification failed"));
    }
    let plaintext = cbc::Decryptor::<aes::Aes256>::new_from_slices(&keys.cipher_key, &keys.iv)
        .expect("key and IV lengths are fixed")
        .decrypt_padded_vec::<Pkcs7>(&message.ciphertext)
        .map_err(|_| SignalError::InvalidMessage("failed to decrypt"))?;
    state.clear_pending_pre_key();
    Ok(plaintext)
}

/// The receiving chain for the sender's ratchet key; a new key is a Diffie-Hellman ratchet
/// step, which also turns our own sending ratchet over.
fn chain_key_for(state: &mut SessionState, theirs: &PublicKey) -> Result<ChainKey, SignalError> {
    if let Some(chain) = state.receiver_chain_key(theirs)? {
        return Ok(chain);
    }
    let (root, receiver_chain) = state
        .root_key()?
        .create_chain(theirs, &state.sender_ratchet_private_key()?)?;
    let ours = KeyPair::generate();
    let (root, sender_chain) = root.create_chain(theirs, &ours.private)?;
    state.set_root_key(&root);
    state.add_receiver_chain(theirs, &receiver_chain);
    let current = state.sender_chain_index()?;
    state.set_previous_counter(current.saturating_sub(1));
    state.set_sender_chain(&ours, &sender_chain);
    Ok(receiver_chain)
}

fn message_key_seed(
    state: &mut SessionState,
    theirs: &PublicKey,
    chain_key: &ChainKey,
    counter: u32,
) -> Result<Vec<u8>, SignalError> {
    if chain_key.index > counter {
        return state
            .take_message_key_seed(theirs, counter)?
            .ok_or(SignalError::Duplicate { counter });
    }
    if counter - chain_key.index > MAX_FORWARD_JUMPS {
        return Err(SignalError::InvalidMessage(
            "message from too far into the future",
        ));
    }
    let mut chain_key = chain_key.clone();
    while chain_key.index < counter {
        state.save_message_key_seed(theirs, chain_key.index, chain_key.message_key_seed())?;
        chain_key = chain_key.next();
    }
    state.set_receiver_chain_key(theirs, &chain_key.next())?;
    Ok(chain_key.message_key_seed().to_vec())
}

/// A `SignalMessage` (WhatsApp's `msg`): version byte, protobuf, 8-byte MAC.
pub(crate) struct SignalMessage {
    version: u8,
    ratchet_key: PublicKey,
    counter: u32,
    ciphertext: Vec<u8>,
    serialized: Vec<u8>,
}

impl SignalMessage {
    fn parse(serialized: &[u8]) -> Result<Self, SignalError> {
        if serialized.len() < MAC_LENGTH + 1 {
            return Err(SignalError::InvalidMessage("too short"));
        }
        let version = check_version(serialized[0])?;
        let wire = WireSignalMessage::decode(&serialized[1..serialized.len() - MAC_LENGTH])
            .map_err(|_| SignalError::InvalidMessage("protobuf"))?;
        if wire.pq_ratchet.is_some_and(|pq| !pq.is_empty()) {
            return Err(SignalError::InvalidMessage(
                "post-quantum ratchet data in a version-3 message",
            ));
        }
        let (Some(ratchet_key), Some(counter), Some(ciphertext)) =
            (wire.ratchet_key, wire.counter, wire.ciphertext)
        else {
            return Err(SignalError::InvalidMessage("missing field"));
        };
        Ok(Self {
            version,
            ratchet_key: PublicKey::deserialize(&ratchet_key)?,
            counter,
            ciphertext,
            serialized: serialized.to_vec(),
        })
    }

    fn verify_mac(&self, sender: &PublicKey, receiver: &PublicKey, mac_key: &[u8; 32]) -> bool {
        let (body, theirs) = self.serialized.split_at(self.serialized.len() - MAC_LENGTH);
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(mac_key).expect("HMAC takes any key length");
        mac.update(&sender.serialize());
        mac.update(&receiver.serialize());
        mac.update(body);
        let ours = mac.finalize().into_bytes();
        bool::from(ours[..MAC_LENGTH].ct_eq(theirs))
    }
}

/// A `PreKeySignalMessage` (WhatsApp's `pkmsg`): the handshake around a first `SignalMessage`.
struct PreKeySignalMessage {
    registration_id: u32,
    pre_key_id: Option<u32>,
    signed_pre_key_id: u32,
    base_key: PublicKey,
    identity_key: PublicKey,
    message: SignalMessage,
}

impl PreKeySignalMessage {
    fn parse(serialized: &[u8]) -> Result<Self, SignalError> {
        let (&first, body) = serialized
            .split_first()
            .ok_or(SignalError::InvalidMessage("empty"))?;
        check_version(first)?;
        let wire = WirePreKeySignalMessage::decode(body)
            .map_err(|_| SignalError::InvalidMessage("protobuf"))?;
        if wire.kyber_pre_key_id.is_some() || wire.kyber_ciphertext.is_some() {
            return Err(SignalError::InvalidMessage(
                "Kyber pre-key in a version-3 message",
            ));
        }
        let (Some(base_key), Some(identity_key), Some(message), Some(signed_pre_key_id)) = (
            wire.base_key,
            wire.identity_key,
            wire.message,
            wire.signed_pre_key_id,
        ) else {
            return Err(SignalError::InvalidMessage("missing field"));
        };
        Ok(Self {
            registration_id: wire.registration_id.unwrap_or(0),
            pre_key_id: wire.pre_key_id,
            signed_pre_key_id,
            base_key: PublicKey::deserialize(&base_key)?,
            identity_key: PublicKey::deserialize(&identity_key)?,
            message: SignalMessage::parse(&message)?,
        })
    }
}

fn check_version(first: u8) -> Result<u8, SignalError> {
    match first >> 4 {
        3 => Ok(3),
        other => Err(SignalError::Version(other)),
    }
}

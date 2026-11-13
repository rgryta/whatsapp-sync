//
// Copyright 2020-2025 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//
// From libsignal v0.74.1, rust/protocol/src/state/session.rs. Modified for whatsapp-sync:
// version 3 only and receive-only, so the post-quantum fields, the sending helpers and the
// logging are gone; skipped message keys are kept as seeds only. The stored form is still
// libsignal's `RecordStructure`, so a record written by libsignal reads here and back.

use prost::Message;
use subtle::ConstantTimeEq;

use super::SignalError;
use super::curve::{KeyPair, PrivateKey, PublicKey};
use super::ratchet::{ChainKey, RootKey};
use crate::proto::{self, RecordStructure, SessionStructure};

pub(super) const VERSION: u32 = 3;
const MAX_RECEIVER_CHAINS: usize = 5;
pub(super) const MAX_MESSAGE_KEYS: usize = 2000;
const ARCHIVED_STATES_MAX_LENGTH: usize = 40;

#[derive(Clone)]
pub(super) struct SessionState(pub SessionStructure);

impl SessionState {
    pub fn new(
        ours: &PublicKey,
        theirs: &PublicKey,
        root: &RootKey,
        alice_base_key: &PublicKey,
    ) -> Self {
        Self(SessionStructure {
            session_version: VERSION,
            local_identity_public: ours.serialize().to_vec(),
            remote_identity_public: theirs.serialize().to_vec(),
            root_key: root.0.to_vec(),
            alice_base_key: alice_base_key.serialize().to_vec(),
            ..Default::default()
        })
    }

    pub fn session_version(&self) -> u32 {
        match self.0.session_version {
            0 => 2,
            v => v,
        }
    }

    pub fn local_identity(&self) -> Result<PublicKey, SignalError> {
        PublicKey::deserialize(&self.0.local_identity_public)
            .map_err(|_| SignalError::InvalidState("local identity"))
    }

    pub fn remote_identity(&self) -> Result<PublicKey, SignalError> {
        PublicKey::deserialize(&self.0.remote_identity_public)
            .map_err(|_| SignalError::InvalidState("remote identity"))
    }

    pub fn root_key(&self) -> Result<RootKey, SignalError> {
        let key = self
            .0
            .root_key
            .as_slice()
            .try_into()
            .map_err(|_| SignalError::InvalidState("root key"))?;
        Ok(RootKey(key))
    }

    pub fn set_root_key(&mut self, root: &RootKey) {
        self.0.root_key = root.0.to_vec();
    }

    pub fn set_previous_counter(&mut self, counter: u32) {
        self.0.previous_counter = counter;
    }

    pub fn sender_ratchet_private_key(&self) -> Result<PrivateKey, SignalError> {
        let chain = self
            .0
            .sender_chain
            .as_ref()
            .ok_or(SignalError::InvalidState("missing sender chain"))?;
        PrivateKey::deserialize(&chain.sender_ratchet_key_private)
            .map_err(|_| SignalError::InvalidState("sender ratchet key"))
    }

    pub fn sender_chain_index(&self) -> Result<u32, SignalError> {
        let chain = self
            .0
            .sender_chain
            .as_ref()
            .ok_or(SignalError::InvalidState("missing sender chain"))?;
        Ok(chain
            .chain_key
            .as_ref()
            .ok_or(SignalError::InvalidState("missing sender chain key"))?
            .index)
    }

    pub fn set_sender_chain(&mut self, ratchet: &KeyPair, chain_key: &ChainKey) {
        self.0.sender_chain = Some(proto::Chain {
            sender_ratchet_key: ratchet.public.serialize().to_vec(),
            sender_ratchet_key_private: ratchet.private.serialize().to_vec(),
            chain_key: Some(proto::ChainKey {
                index: chain_key.index,
                key: chain_key.key.to_vec(),
            }),
            message_keys: Vec::new(),
        });
    }

    fn receiver_chain(&self, sender: &PublicKey) -> Result<Option<usize>, SignalError> {
        for (index, chain) in self.0.receiver_chains.iter().enumerate() {
            let key = PublicKey::deserialize(&chain.sender_ratchet_key)
                .map_err(|_| SignalError::InvalidState("receiver chain ratchet key"))?;
            if &key == sender {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub fn receiver_chain_key(&self, sender: &PublicKey) -> Result<Option<ChainKey>, SignalError> {
        let Some(index) = self.receiver_chain(sender)? else {
            return Ok(None);
        };
        let chain_key = self.0.receiver_chains[index]
            .chain_key
            .as_ref()
            .ok_or(SignalError::InvalidState("missing receiver chain key"))?;
        let key = chain_key
            .key
            .as_slice()
            .try_into()
            .map_err(|_| SignalError::InvalidState("receiver chain key"))?;
        Ok(Some(ChainKey {
            key,
            index: chain_key.index,
        }))
    }

    pub fn add_receiver_chain(&mut self, sender: &PublicKey, chain_key: &ChainKey) {
        self.0.receiver_chains.push(proto::Chain {
            sender_ratchet_key: sender.serialize().to_vec(),
            sender_ratchet_key_private: Vec::new(),
            chain_key: Some(proto::ChainKey {
                index: chain_key.index,
                key: chain_key.key.to_vec(),
            }),
            message_keys: Vec::new(),
        });
        if self.0.receiver_chains.len() > MAX_RECEIVER_CHAINS {
            self.0.receiver_chains.remove(0);
        }
    }

    pub fn set_receiver_chain_key(
        &mut self,
        sender: &PublicKey,
        chain_key: &ChainKey,
    ) -> Result<(), SignalError> {
        let index = self
            .receiver_chain(sender)?
            .ok_or(SignalError::InvalidState("no such receiver chain"))?;
        self.0.receiver_chains[index].chain_key = Some(proto::ChainKey {
            index: chain_key.index,
            key: chain_key.key.to_vec(),
        });
        Ok(())
    }

    /// Takes a skipped message's key seed out of the chain, if it is there.
    pub fn take_message_key_seed(
        &mut self,
        sender: &PublicKey,
        counter: u32,
    ) -> Result<Option<Vec<u8>>, SignalError> {
        let Some(index) = self.receiver_chain(sender)? else {
            return Ok(None);
        };
        let keys = &mut self.0.receiver_chains[index].message_keys;
        let Some(position) = keys.iter().position(|k| k.index == counter) else {
            return Ok(None);
        };
        let key = keys.remove(position);
        if key.seed.is_empty() {
            return Err(SignalError::InvalidState(
                "skipped message key without a seed",
            ));
        }
        Ok(Some(key.seed))
    }

    /// Newest first, as libsignal keeps them; past the limit the oldest is dropped.
    pub fn save_message_key_seed(
        &mut self,
        sender: &PublicKey,
        counter: u32,
        seed: [u8; 32],
    ) -> Result<(), SignalError> {
        let index = self
            .receiver_chain(sender)?
            .ok_or(SignalError::InvalidState("no such receiver chain"))?;
        let keys = &mut self.0.receiver_chains[index].message_keys;
        keys.insert(
            0,
            proto::MessageKey {
                index: counter,
                seed: seed.to_vec(),
            },
        );
        keys.truncate(MAX_MESSAGE_KEYS);
        Ok(())
    }

    pub fn clear_pending_pre_key(&mut self) {
        self.0.pending_pre_key = None;
    }

    pub fn set_registration_ids(&mut self, local: u32, remote: u32) {
        self.0.local_registration_id = local;
        self.0.remote_registration_id = remote;
    }
}

/// One contact device's sessions: the current one and up to 40 archived, newest first.
#[derive(Clone, Default)]
pub struct SessionRecord {
    pub(super) current: Option<SessionState>,
    /// Encoded `SessionStructure`s, decoded only when a message needs them.
    pub(super) previous: Vec<Vec<u8>>,
}

impl SessionRecord {
    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        let record = RecordStructure::decode(bytes)
            .map_err(|_| SignalError::InvalidState("session record encoding"))?;
        Ok(Self {
            current: record.current_session.map(SessionState),
            previous: record.previous_sessions,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        RecordStructure {
            current_session: self.current.as_ref().map(|s| s.0.clone()),
            previous_sessions: self.previous.clone(),
        }
        .encode_to_vec()
    }

    pub fn has_current_session(&self) -> bool {
        self.current.is_some()
    }

    pub fn archived_sessions(&self) -> usize {
        self.previous.len()
    }

    pub(super) fn previous_state(&self, index: usize) -> Result<SessionState, SignalError> {
        SessionStructure::decode(self.previous[index].as_slice())
            .map(SessionState)
            .map_err(|_| SignalError::InvalidState("archived session encoding"))
    }

    /// Makes the session started by `alice_base_key` current, if this record has it.
    pub(super) fn promote_matching_session(
        &mut self,
        alice_base_key: &[u8],
    ) -> Result<bool, SignalError> {
        if let Some(current) = &self.current
            && current.session_version() == VERSION
            && bool::from(alice_base_key.ct_eq(&current.0.alice_base_key))
        {
            return Ok(true);
        }
        for index in 0..self.previous.len() {
            let state = self.previous_state(index)?;
            if state.session_version() == VERSION
                && bool::from(alice_base_key.ct_eq(&state.0.alice_base_key))
            {
                self.promote_old_session(index, state);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn promote_old_session(&mut self, index: usize, state: SessionState) {
        self.previous.remove(index);
        self.promote_state(state);
    }

    pub(super) fn promote_state(&mut self, state: SessionState) {
        if let Some(mut current) = self.current.take() {
            if self.previous.len() >= ARCHIVED_STATES_MAX_LENGTH {
                self.previous.pop();
            }
            current.clear_pending_pre_key();
            self.previous.insert(0, current.0.encode_to_vec());
        }
        self.current = Some(state);
    }
}

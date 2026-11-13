//! WhatsApp's group ("sender key") cipher.
//!
//! Each group member has a chain per group. Its key steps forward once per message: HMAC
//! with `0x01` gives that message's key seed, HMAC with `0x02` gives the next chain key. The
//! seed expands (HKDF, "WhisperGroup") into an AES-256-CBC key and IV, and every message is
//! signed by the member's signing key. This is the original libsignal design that Baileys
//! and whatsmeow implement. libsignal has the same cryptography under a wire format WhatsApp
//! does not use, which is why it is written here rather than vendored with `signal`.

use aes::cipher::{KeyIvInit, block_padding::Pkcs7};
use cbc::cipher::BlockModeDecrypt;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use prost::Message;
use sha2::Sha256;

use crate::proto::{
    SenderChainKey, SenderKeyRecordStructure, SenderKeyStateStructure, SenderSigningKey,
    WaSenderKeyDistributionMessage, WaSenderKeyMessage,
};
use crate::signal::curve::PublicKey;

/// The only group message version WhatsApp sends: the high nibble of the first byte.
const VERSION: u8 = 3;
const SIGNATURE_LEN: usize = 64;
/// How far ahead of the chain a message may be, and how many skipped keys are kept. The
/// same bound as libsignal's, so a forged iteration cannot make us derive without limit.
const MAX_FORWARD: u32 = 2000;
/// States kept per member and group; an older one is dropped when a new one arrives.
const MAX_STATES: usize = 5;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GroupError {
    #[error("not a version-3 group message")]
    Version,
    #[error("malformed group message")]
    Malformed,
    #[error("no sender key with this id; the distribution message has not arrived")]
    NoState,
    #[error("signature does not verify")]
    Signature,
    #[error("message {0} was already decrypted")]
    Duplicate(u32),
    #[error("message is {0} ahead of the chain, more than allowed")]
    TooFarAhead(u32),
    #[error("ciphertext does not decrypt")]
    Decrypt,
}

#[derive(Clone)]
pub struct SenderKeyState {
    pub id: u32,
    /// The iteration `chain_key` derives next.
    pub iteration: u32,
    pub chain_key: [u8; 32],
    pub signing_key: PublicKey,
    /// Seeds of skipped messages, oldest first.
    pub message_keys: Vec<(u32, [u8; 32])>,
}

/// One member's sender keys in one group, newest first.
#[derive(Clone, Default)]
pub struct SenderKeyRecord {
    pub states: Vec<SenderKeyState>,
}

impl SenderKeyRecord {
    /// Takes in a member's `SenderKeyDistributionMessage`, the start of their chain.
    pub fn process_distribution(&mut self, serialized: &[u8]) -> Result<(), GroupError> {
        let body = versioned(serialized)?;
        let message =
            WaSenderKeyDistributionMessage::decode(body).map_err(|_| GroupError::Malformed)?;
        let (Some(id), Some(iteration), Some(chain_key), Some(signing_key)) = (
            message.id,
            message.iteration,
            message.chain_key,
            message.signing_key,
        ) else {
            return Err(GroupError::Malformed);
        };
        let chain_key = chain_key.try_into().map_err(|_| GroupError::Malformed)?;
        let signing_key =
            PublicKey::deserialize(&signing_key).map_err(|_| GroupError::Malformed)?;
        // A repeat of a key we hold must not rewind its chain: that would reopen every
        // message already decrypted to a replay.
        if self
            .states
            .iter()
            .any(|s| s.id == id && s.signing_key == signing_key)
        {
            return Ok(());
        }
        self.states.insert(
            0,
            SenderKeyState {
                id,
                iteration,
                chain_key,
                signing_key,
                message_keys: Vec::new(),
            },
        );
        self.states.truncate(MAX_STATES);
        Ok(())
    }

    /// The stored form: libsignal's `SenderKeyRecordStructure`, public signing keys only.
    pub fn serialize(&self) -> Vec<u8> {
        SenderKeyRecordStructure {
            states: self
                .states
                .iter()
                .map(|s| SenderKeyStateStructure {
                    chain_id: s.id,
                    chain_key: Some(SenderChainKey {
                        iteration: s.iteration,
                        seed: s.chain_key.to_vec(),
                    }),
                    signing_key: Some(SenderSigningKey {
                        public: s.signing_key.serialize().to_vec(),
                    }),
                    message_keys: s
                        .message_keys
                        .iter()
                        .map(|(iteration, seed)| SenderChainKey {
                            iteration: *iteration,
                            seed: seed.to_vec(),
                        })
                        .collect(),
                })
                .collect(),
        }
        .encode_to_vec()
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, GroupError> {
        let record = SenderKeyRecordStructure::decode(bytes).map_err(|_| GroupError::Malformed)?;
        let seed = |bytes: &[u8]| <[u8; 32]>::try_from(bytes).map_err(|_| GroupError::Malformed);
        let states = record
            .states
            .into_iter()
            .map(|s| {
                let chain = s.chain_key.ok_or(GroupError::Malformed)?;
                let signing = s.signing_key.ok_or(GroupError::Malformed)?;
                Ok(SenderKeyState {
                    id: s.chain_id,
                    iteration: chain.iteration,
                    chain_key: seed(&chain.seed)?,
                    signing_key: PublicKey::deserialize(&signing.public)
                        .map_err(|_| GroupError::Malformed)?,
                    message_keys: s
                        .message_keys
                        .iter()
                        .map(|k| Ok((k.iteration, seed(&k.seed)?)))
                        .collect::<Result<_, GroupError>>()?,
                })
            })
            .collect::<Result<_, GroupError>>()?;
        Ok(Self { states })
    }

    /// Decrypts a `SenderKeyMessage`. The record only changes when this succeeds.
    pub fn decrypt(&mut self, serialized: &[u8]) -> Result<Vec<u8>, GroupError> {
        let signed_len = serialized
            .len()
            .checked_sub(SIGNATURE_LEN)
            .ok_or(GroupError::Malformed)?;
        let (signed, signature) = serialized.split_at(signed_len);
        let message =
            WaSenderKeyMessage::decode(versioned(signed)?).map_err(|_| GroupError::Malformed)?;
        let (Some(id), Some(iteration), Some(ciphertext)) =
            (message.id, message.iteration, message.ciphertext)
        else {
            return Err(GroupError::Malformed);
        };
        let position = self
            .states
            .iter()
            .position(|s| s.id == id)
            .ok_or(GroupError::NoState)?;
        let mut state = self.states[position].clone();
        if !state.signing_key.verify_signature(signed, signature) {
            return Err(GroupError::Signature);
        }
        let seed = state.take_seed(iteration)?;
        let plaintext = decrypt_with_seed(&seed, &ciphertext)?;
        self.states[position] = state;
        Ok(plaintext)
    }
}

impl SenderKeyState {
    fn take_seed(&mut self, iteration: u32) -> Result<[u8; 32], GroupError> {
        if iteration < self.iteration {
            let at = self
                .message_keys
                .iter()
                .position(|(i, _)| *i == iteration)
                .ok_or(GroupError::Duplicate(iteration))?;
            return Ok(self.message_keys.remove(at).1);
        }
        let ahead = iteration - self.iteration;
        if ahead > MAX_FORWARD {
            return Err(GroupError::TooFarAhead(ahead));
        }
        while self.iteration < iteration {
            self.message_keys
                .push((self.iteration, hmac(&self.chain_key, 0x01)));
            self.step();
        }
        let excess = self.message_keys.len().saturating_sub(MAX_FORWARD as usize);
        self.message_keys.drain(..excess);
        let seed = hmac(&self.chain_key, 0x01);
        self.step();
        Ok(seed)
    }

    fn step(&mut self) {
        self.chain_key = hmac(&self.chain_key, 0x02);
        self.iteration += 1;
    }
}

fn versioned(serialized: &[u8]) -> Result<&[u8], GroupError> {
    match serialized.split_first() {
        Some((version, body)) if version >> 4 == VERSION => Ok(body),
        Some(_) => Err(GroupError::Version),
        None => Err(GroupError::Malformed),
    }
}

fn hmac(key: &[u8; 32], input: u8) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(&[input]);
    mac.finalize().into_bytes().into()
}

fn decrypt_with_seed(seed: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, GroupError> {
    // `None` is libsignal's salt: 32 zero bytes.
    let mut okm = [0u8; 48];
    Hkdf::<Sha256>::new(None, seed)
        .expand(b"WhisperGroup", &mut okm)
        .expect("48 bytes is a valid HKDF length");
    let (iv, key) = okm.split_at(16);
    cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
        .expect("key and IV lengths are fixed")
        .decrypt_padded_vec::<Pkcs7>(ciphertext)
        .map_err(|_| GroupError::Decrypt)
}

//! Reads WAHA's session, Baileys' multi-file auth directory, into a [`Device`].
//!
//! This is how the client takes over WAHA's linked device instead of linking a new one. Every
//! key is checked as it is read: a public key must be the one its private key gives, the
//! signed pre-key must carry the identity's signature, and the account record must carry both
//! the phone's and this device's signatures. A wrong byte offset or encoding fails here rather
//! than on WhatsApp's servers, where a broken identity can cost the pairing.
//!
//! Sessions are Baileys' libsignal-node `SessionRecord` JSON. They become libsignal's stored
//! form field by field (see [`session_record`]). `wacore-libsignal`'s `legacy_session.rs` maps
//! the same model and was the cross-check for the mapping.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use prost::Message;
use rand::Rng;
use serde::Deserialize;
use serde::de::{MapAccess, Visitor};

use crate::group::{SenderKeyRecord, SenderKeyState};
use crate::proto;
use crate::signal::curve::{KeyPair, PrivateKey, PublicKey};
use crate::signal::{Address, SessionRecord, Store};

/// libsignal keeps this many archived sessions per address.
const ARCHIVED_SESSIONS_MAX: usize = 40;
const CURRENT_SESSION: i64 = -1;
const SENDING: u8 = 1;
const RECEIVING: u8 = 2;
/// Prefixes of the `account` signatures in WhatsApp's device pairing (ADV).
const ACCOUNT_SIGNATURE_PREFIX: [u8; 2] = [6, 0];
const DEVICE_SIGNATURE_PREFIX: [u8; 2] = [6, 1];

#[derive(Debug, thiserror::Error)]
#[error("{file}: {reason}")]
pub struct ImportError {
    pub file: String,
    pub reason: String,
}

fn fail(file: &str, reason: impl fmt::Display) -> ImportError {
    ImportError {
        file: file.to_owned(),
        reason: reason.to_string(),
    }
}

/// Everything WAHA knew about its linked device that this client needs to be it.
pub struct Device {
    pub noise: KeyPair,
    pub identity: KeyPair,
    pub registration_id: u32,
    pub signed_pre_key: SignedPreKey,
    pub adv_secret: Vec<u8>,
    /// Absent until a device has been paired.
    pub account: Option<Account>,
    pub me: Option<Me>,
    pub platform: Option<String>,
    pub routing_info: Option<Vec<u8>>,
    pub next_pre_key_id: u32,
    pub first_unuploaded_pre_key_id: u32,
    pub pre_keys: Vec<(u32, KeyPair)>,
    pub sessions: Vec<(Address, SessionRecord)>,
    /// The identity key each contact device last used, trusted on first use as WAHA did.
    pub identities: Vec<(Address, PublicKey)>,
    /// Keyed by group JID and sending device.
    pub sender_keys: HashMap<(String, Address), SenderKeyRecord>,
    /// App-state sync keys by key id, for contact names and chat settings later.
    pub app_state_keys: BTreeMap<String, Vec<u8>>,
    /// Phone-number user to LID user.
    pub lid_mappings: BTreeMap<String, String>,
    pub stats: ImportStats,
}

pub struct SignedPreKey {
    pub id: u32,
    pub key_pair: KeyPair,
    pub signature: Vec<u8>,
}

/// The phone's signature over this device, from pairing.
pub struct Account {
    pub details: Vec<u8>,
    pub account_signature_key: Vec<u8>,
    pub account_signature: Vec<u8>,
    pub device_signature: Vec<u8>,
}

pub struct Me {
    pub id: String,
    pub lid: Option<String>,
    pub name: Option<String>,
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub current_sessions: usize,
    pub archived_sessions: usize,
    /// Archived sessions beyond libsignal's limit, dropped.
    pub dropped_sessions: usize,
    /// Closed receiving chains: Baileys deletes their chain key and keeps only skipped
    /// message keys, so the key here is random (see [`chain`]).
    pub closed_chains: usize,
    /// Files read but not carried over, by kind. The server sends these again.
    pub not_imported: BTreeMap<String, usize>,
}

impl Device {
    /// The keys and sessions pairwise decryption needs.
    pub fn signal_store(&self) -> Store {
        Store {
            identity: self.identity.clone(),
            registration_id: self.registration_id,
            signed_pre_keys: [(self.signed_pre_key.id, self.signed_pre_key.key_pair.clone())]
                .into(),
            pre_keys: self.pre_keys.iter().cloned().collect(),
            sessions: self.sessions.iter().cloned().collect(),
            identities: self.identities.iter().cloned().collect(),
            used_pre_keys: Vec::new(),
        }
    }
}

/// Reads a Baileys multi-file auth directory.
pub fn import(dir: &Path) -> Result<Device, ImportError> {
    let creds_file = "creds.json";
    let creds: Creds = read_json(dir, creds_file)?;
    let creds_err = |reason: String| fail(creds_file, reason);

    let noise = key_pair(&creds.noise_key).map_err(|e| creds_err(format!("noiseKey: {e}")))?;
    let identity = key_pair(&creds.signed_identity_key)
        .map_err(|e| creds_err(format!("signedIdentityKey: {e}")))?;
    let signed_pre_key = SignedPreKey {
        id: creds.signed_pre_key.key_id,
        key_pair: key_pair(&creds.signed_pre_key.key_pair)
            .map_err(|e| creds_err(format!("signedPreKey: {e}")))?,
        signature: creds.signed_pre_key.signature.bytes().map_err(creds_err)?,
    };
    if !identity.public.verify_signature(
        &signed_pre_key.key_pair.public.serialize(),
        &signed_pre_key.signature,
    ) {
        return Err(creds_err(
            "signedPreKey: the identity key did not sign it".into(),
        ));
    }
    let account = creds
        .account
        .as_ref()
        .map(|a| -> Result<Account, String> {
            Ok(Account {
                details: a.details.bytes()?,
                account_signature_key: a.account_signature_key.bytes()?,
                account_signature: a.account_signature.bytes()?,
                device_signature: a.device_signature.bytes()?,
            })
        })
        .transpose()
        .map_err(creds_err)?;
    if let Some(account) = &account {
        verify_account(account, &identity).map_err(creds_err)?;
    }
    let local_identity = identity.public.serialize().to_vec();

    let mut device = Device {
        noise,
        identity,
        registration_id: creds.registration_id,
        signed_pre_key,
        adv_secret: B64
            .decode(&creds.adv_secret_key)
            .map_err(|e| creds_err(format!("advSecretKey: {e}")))?,
        account,
        me: creds.me.map(|me| Me {
            id: me.id,
            lid: me.lid,
            name: me.name,
        }),
        platform: creds.platform,
        routing_info: creds
            .routing_info
            .map(|r| r.bytes())
            .transpose()
            .map_err(creds_err)?,
        next_pre_key_id: creds.next_pre_key_id,
        first_unuploaded_pre_key_id: creds.first_unuploaded_pre_key_id,
        pre_keys: Vec::new(),
        sessions: Vec::new(),
        identities: Vec::new(),
        sender_keys: HashMap::new(),
        app_state_keys: BTreeMap::new(),
        lid_mappings: BTreeMap::new(),
        stats: ImportStats::default(),
    };

    let entries = std::fs::read_dir(dir).map_err(|e| fail(&dir.display().to_string(), e))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| fail(&dir.display().to_string(), e))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();

    for name in names {
        let Some(stem) = name.strip_suffix(".json") else {
            *device.stats.not_imported.entry(name).or_default() += 1;
            continue;
        };
        if stem == "creds" {
        } else if let Some(id) = stem.strip_prefix("pre-key-") {
            let pair: Pair = read_json(dir, &name)?;
            let id = id.parse().map_err(|e| fail(&name, e))?;
            device
                .pre_keys
                .push((id, key_pair(&pair).map_err(|e| fail(&name, e))?));
        } else if let Some(address) = stem.strip_prefix("session-") {
            let file: SessionFile = read_json(dir, &name)?;
            let record = session_record(
                &file,
                &local_identity,
                creds.registration_id,
                &mut device.stats,
            )
            .map_err(|e| fail(&name, e))?;
            device
                .sessions
                .push((signal_address(address).map_err(|e| fail(&name, e))?, record));
        } else if let Some(address) = stem.strip_prefix("identity-key-") {
            let key: Bin = read_json(dir, &name)?;
            let key = PublicKey::deserialize(&key.bytes().map_err(|e| fail(&name, e))?)
                .map_err(|e| fail(&name, e))?;
            device
                .identities
                .push((signal_address(address).map_err(|e| fail(&name, e))?, key));
        } else if stem.starts_with("sender-key-memory-") {
            *device
                .stats
                .not_imported
                .entry("sender-key-memory".into())
                .or_default() += 1;
        } else if let Some(key_name) = stem.strip_prefix("sender-key-") {
            let (group, sender) = sender_key_name(key_name).map_err(|e| fail(&name, e))?;
            let raw: Bin = read_json(dir, &name)?;
            let states: Vec<NodeSenderKeyState> =
                serde_json::from_slice(&raw.bytes().map_err(|e| fail(&name, e))?)
                    .map_err(|e| fail(&name, e))?;
            let record = sender_key_record(&states).map_err(|e| fail(&name, e))?;
            device.sender_keys.insert((group, sender), record);
        } else if let Some(id) = stem.strip_prefix("app-state-sync-key-") {
            let key: AppStateKey = read_json(dir, &name)?;
            let data = key.key_data.bytes().map_err(|e| fail(&name, e))?;
            device.app_state_keys.insert(unfix(id), data);
        } else if let Some(user) = stem.strip_prefix("lid-mapping-") {
            if !user.ends_with("_reverse") {
                let lid: String = read_json(dir, &name)?;
                device.lid_mappings.insert(user.to_owned(), lid);
            }
        } else {
            let kind = stem
                .split(|c: char| c.is_ascii_digit() || c == '@')
                .next()
                .unwrap_or(stem);
            *device
                .stats
                .not_imported
                .entry(kind.trim_end_matches('-').to_owned())
                .or_default() += 1;
        }
    }
    Ok(device)
}

/// The two signatures pairing leaves on a device: the phone's over this device's identity,
/// and this device's over the phone's. `details` is what both cover.
fn verify_account(account: &Account, identity: &KeyPair) -> Result<(), String> {
    let ours: &[u8] = identity.public.bytes();
    let phone = public_key(&account.account_signature_key)
        .map_err(|e| format!("accountSignatureKey: {e}"))?;
    let signed = [&ACCOUNT_SIGNATURE_PREFIX[..], &account.details, ours].concat();
    if !phone.verify_signature(&signed, &account.account_signature) {
        return Err("account: the phone's signature over this device does not verify".into());
    }
    let signed = [
        &DEVICE_SIGNATURE_PREFIX[..],
        &account.details,
        ours,
        phone.bytes(),
    ]
    .concat();
    if !identity
        .public
        .verify_signature(&signed, &account.device_signature)
    {
        return Err("account: this device's signature does not verify".into());
    }
    Ok(())
}

/// One address's sessions, libsignal-node v1 to libsignal.
///
/// libsignal-node keeps every session in one map keyed by base key, the open one marked
/// `closed: -1`; libsignal keeps one current session and the archived ones newest first.
pub fn session_record(
    file: &SessionFile,
    local_identity: &[u8],
    local_registration_id: u32,
    stats: &mut ImportStats,
) -> Result<SessionRecord, String> {
    let mut current = None;
    let mut archived = Vec::new();
    for (index_key, session) in &file.sessions.0 {
        if *index_key != session.index_info.base_key {
            return Err("a session is filed under a base key that is not its own".into());
        }
        let structure = session_structure(session, local_identity, local_registration_id, stats)?;
        match session.index_info.closed {
            CURRENT_SESSION if current.is_some() => return Err("two open sessions".into()),
            CURRENT_SESSION => current = Some(structure),
            closed if closed >= 0 => archived.push((closed, structure)),
            closed => return Err(format!("closed timestamp {closed}")),
        }
    }
    archived.sort_by_key(|(closed, _)| std::cmp::Reverse(*closed));
    stats.dropped_sessions += archived.len().saturating_sub(ARCHIVED_SESSIONS_MAX);
    archived.truncate(ARCHIVED_SESSIONS_MAX);
    stats.current_sessions += usize::from(current.is_some());
    stats.archived_sessions += archived.len();
    let record = proto::RecordStructure {
        current_session: current,
        previous_sessions: archived
            .into_iter()
            .map(|(_, s)| s.encode_to_vec())
            .collect(),
    };
    SessionRecord::deserialize(&record.encode_to_vec()).map_err(|e| e.to_string())
}

fn session_structure(
    session: &NodeSession,
    local_identity: &[u8],
    local_registration_id: u32,
    stats: &mut ImportStats,
) -> Result<proto::SessionStructure, String> {
    let ratchet = &session.current_ratchet;
    let our_ratchet_key = b64(&ratchet.ephemeral_key_pair.pub_key)?;
    let our_ratchet_private = b64(&ratchet.ephemeral_key_pair.priv_key)?;
    let mut sender_chain = None;
    let mut receiver_chains = Vec::new();
    for (ratchet_key, node) in &session.chains.0 {
        let mut built = chain(b64(ratchet_key)?, node, stats)?;
        match node.chain_type {
            SENDING if built.sender_ratchet_key != our_ratchet_key => {
                return Err("a sending chain that is not the current ratchet key".into());
            }
            SENDING if sender_chain.is_some() => return Err("two sending chains".into()),
            SENDING => {
                built.sender_ratchet_key_private = our_ratchet_private.clone();
                sender_chain = Some(built);
            }
            RECEIVING => receiver_chains.push(built),
            other => return Err(format!("chain type {other}")),
        }
    }
    // libsignal-node always holds a sending chain for the current ratchet key; if one is ever
    // missing, libsignal still needs the key pair, because the next DH ratchet step on receive
    // derives from it. shortcut: its chain key is random, so this receive-only client could not
    // send on it; a send path would have to ratchet first.
    let sender_chain = sender_chain.unwrap_or_else(|| proto::Chain {
        sender_ratchet_key: our_ratchet_key,
        sender_ratchet_key_private: our_ratchet_private,
        chain_key: Some(proto::ChainKey {
            index: 0,
            key: random_key(),
        }),
        message_keys: Vec::new(),
    });
    let pending_pre_key = session
        .pending_pre_key
        .as_ref()
        .map(|p| -> Result<proto::PendingPreKey, String> {
            Ok(proto::PendingPreKey {
                pre_key_id: p.pre_key_id,
                base_key: b64(&p.base_key)?,
                signed_pre_key_id: i32::try_from(p.signed_key_id).map_err(|e| e.to_string())?,
                timestamp: session.index_info.created,
            })
        })
        .transpose()?;
    Ok(proto::SessionStructure {
        session_version: 3,
        local_identity_public: local_identity.to_vec(),
        remote_identity_public: b64(&session.index_info.remote_identity_key)?,
        root_key: b64(&ratchet.root_key)?,
        previous_counter: u32::try_from(ratchet.previous_counter).map_err(|e| e.to_string())?,
        sender_chain: Some(sender_chain),
        receiver_chains,
        pending_pre_key,
        remote_registration_id: session.registration_id,
        local_registration_id,
        alice_base_key: b64(&session.index_info.base_key)?,
    })
}

/// libsignal-node's `counter` is the last index derived, starting at -1; libsignal's
/// `index` is the next one, so it is always `counter + 1`.
fn chain(
    ratchet_key: Vec<u8>,
    node: &NodeChain,
    stats: &mut ImportStats,
) -> Result<proto::Chain, String> {
    let index = u32::try_from(node.chain_key.counter + 1)
        .map_err(|_| format!("chain counter {}", node.chain_key.counter))?;
    let key = match &node.chain_key.key {
        Some(key) => b64(key)?,
        // A closed chain. libsignal requires a chain key, and what matters is that nobody else
        // knows it: a fixed filler would let anyone who read this code forge messages on the
        // chain. A random one derives keys that never authenticate, which is how libsignal-node
        // treats the chain too: only its skipped message keys still decrypt.
        None => {
            stats.closed_chains += 1;
            random_key()
        }
    };
    let message_keys = node
        .message_keys
        .iter()
        .map(|(index, seed)| {
            Ok(proto::MessageKey {
                index: index
                    .parse()
                    .map_err(|_| format!("message key index {index}"))?,
                seed: b64(seed)?,
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(proto::Chain {
        sender_ratchet_key: ratchet_key,
        sender_ratchet_key_private: Vec::new(),
        chain_key: Some(proto::ChainKey { index, key }),
        message_keys,
    })
}

/// Baileys keeps a member's states oldest first; ours are newest first.
fn sender_key_record(states: &[NodeSenderKeyState]) -> Result<SenderKeyRecord, String> {
    let states = states
        .iter()
        .rev()
        .map(|s| {
            let message_keys = s
                .sender_message_keys
                .iter()
                .map(|k| Ok((k.iteration, key32(&k.seed.bytes()?)?)))
                .collect::<Result<Vec<_>, String>>()?;
            Ok(SenderKeyState {
                id: s.sender_key_id,
                iteration: s.sender_chain_key.iteration,
                chain_key: key32(&s.sender_chain_key.seed.bytes()?)?,
                signing_key: public_key(&s.sender_signing_key.public.bytes()?)?,
                message_keys,
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(SenderKeyRecord { states })
}

/// `user[_domain].device`, as Baileys names a Signal address.
fn signal_address(text: &str) -> Result<Address, String> {
    let (name, device) = text.rsplit_once('.').ok_or("address has no device")?;
    let device = device
        .parse()
        .map_err(|_| "address device is not a number")?;
    Ok(Address {
        name: name.to_owned(),
        device,
    })
}

/// `group::user::device` with Baileys' `:` to `-` file-name substitution.
fn sender_key_name(text: &str) -> Result<(String, Address), String> {
    let mut parts = text.rsplitn(3, "--");
    let (Some(device), Some(name), Some(group)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("sender key name is not group--user--device".into());
    };
    Ok((
        group.to_owned(),
        signal_address(&format!("{name}.{device}"))?,
    ))
}

/// Undoes Baileys' `/` to `__` file-name substitution, the one that can appear in a key id.
fn unfix(name: &str) -> String {
    name.replace("__", "/")
}

fn key_pair(pair: &Pair) -> Result<KeyPair, String> {
    let private = PrivateKey::deserialize(&pair.private.bytes()?).map_err(|e| e.to_string())?;
    let public = public_key(&pair.public.bytes()?)?;
    if private.public_key() != public {
        return Err("the public key is not the private key's".into());
    }
    Ok(KeyPair { public, private })
}

/// Baileys stores Curve25519 public keys both bare (32 bytes) and with libsignal's type byte.
fn public_key(bytes: &[u8]) -> Result<PublicKey, String> {
    match bytes.len() {
        32 => PublicKey::from_djb(bytes),
        _ => PublicKey::deserialize(bytes),
    }
    .map_err(|e| e.to_string())
}

fn key32(bytes: &[u8]) -> Result<[u8; 32], String> {
    bytes
        .try_into()
        .map_err(|_| format!("expected 32 bytes, got {}", bytes.len()))
}

fn b64(text: &str) -> Result<Vec<u8>, String> {
    B64.decode(text).map_err(|e| e.to_string())
}

fn random_key() -> Vec<u8> {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key.to_vec()
}

fn read_json<T: for<'de> Deserialize<'de>>(dir: &Path, name: &str) -> Result<T, ImportError> {
    let bytes = std::fs::read(dir.join(name)).map_err(|e| fail(name, e))?;
    serde_json::from_slice(&bytes).map_err(|e| fail(name, e))
}

// --- Baileys' JSON ---------------------------------------------------------------------

/// A byte string as Baileys writes it: `{"type": "Buffer", "data": <base64 or byte array>}`
/// from its `BufferJSON`, or a bare base64 string where protobuf.js wrote the object.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Bin {
    Buffer { data: BufferData },
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum BufferData {
    Base64(String),
    Bytes(Vec<u8>),
}

impl Bin {
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Bin::Buffer {
                data: BufferData::Bytes(bytes),
            } => Ok(bytes.clone()),
            Bin::Buffer {
                data: BufferData::Base64(text),
            }
            | Bin::Text(text) => b64(text),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Creds {
    noise_key: Pair,
    signed_identity_key: Pair,
    signed_pre_key: NodeSignedPreKey,
    registration_id: u32,
    adv_secret_key: String,
    #[serde(default)]
    next_pre_key_id: u32,
    #[serde(default)]
    first_unuploaded_pre_key_id: u32,
    account: Option<NodeAccount>,
    me: Option<NodeMe>,
    platform: Option<String>,
    routing_info: Option<Bin>,
}

#[derive(Deserialize)]
struct Pair {
    private: Bin,
    public: Bin,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeSignedPreKey {
    key_pair: Pair,
    signature: Bin,
    key_id: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeAccount {
    details: Bin,
    account_signature_key: Bin,
    account_signature: Bin,
    device_signature: Bin,
}

#[derive(Deserialize)]
struct NodeMe {
    id: String,
    lid: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
pub struct SessionFile {
    #[serde(rename = "_sessions")]
    sessions: Ordered<NodeSession>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeSession {
    registration_id: u32,
    current_ratchet: NodeRatchet,
    index_info: NodeIndexInfo,
    #[serde(rename = "_chains")]
    chains: Ordered<NodeChain>,
    pending_pre_key: Option<NodePendingPreKey>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeRatchet {
    ephemeral_key_pair: NodeKeyPair,
    previous_counter: i64,
    root_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeKeyPair {
    pub_key: String,
    priv_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeIndexInfo {
    base_key: String,
    closed: i64,
    created: u64,
    remote_identity_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeChain {
    chain_key: NodeChainKey,
    chain_type: u8,
    message_keys: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct NodeChainKey {
    counter: i64,
    key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodePendingPreKey {
    pre_key_id: Option<u32>,
    signed_key_id: u32,
    base_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeSenderKeyState {
    sender_key_id: u32,
    sender_chain_key: NodeSenderChainKey,
    sender_signing_key: NodeSigningKey,
    #[serde(default)]
    sender_message_keys: Vec<NodeSenderChainKey>,
}

#[derive(Deserialize)]
struct NodeSenderChainKey {
    iteration: u32,
    seed: Bin,
}

#[derive(Deserialize)]
struct NodeSigningKey {
    public: Bin,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppStateKey {
    key_data: Bin,
}

/// A JSON object read in file order. libsignal trims the oldest receiving chain first, so
/// the order libsignal-node wrote its chains in has to survive the import.
pub struct Ordered<V>(Vec<(String, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Entries<V> {
            type Value = Ordered<V>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }
        deserializer.deserialize_map(Entries(std::marker::PhantomData))
    }
}

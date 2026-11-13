//! Keeping the server stocked with one-time pre-keys.
//!
//! A contact's first message to this device consumes one of them. When the server runs out,
//! new contacts can only start sessions without one, so Baileys tops the stock up whenever it
//! is at or below [`MIN_PRE_KEYS`]. Ported from `Utils/signal.js` (`generateOrGetPreKeys`,
//! `getNextPreKeys`, `getNextPreKeysNode`); `tests/stanza.rs` checks the upload's bytes.

use std::ops::Range;

use crate::baileys::SignedPreKey;
use crate::binary::{Content, Node, Value};
use crate::signal::curve::{KeyPair, PublicKey};
use crate::stanza::{bytes, pre_key_node, server, signed_pre_key_node};

/// Baileys' `MIN_PREKEY_COUNT`: top up at or below this many on the server.
pub const MIN_PRE_KEYS: u32 = 5;
/// Baileys' `INITIAL_PREKEY_COUNT`: what to upload when the server has none at all.
pub const INITIAL_PRE_KEYS: u32 = 812;

/// One upload: the keys to create, which ids to send, and the counters after it. No `Debug`:
/// it holds private keys.
pub struct Plan {
    pub new_keys: Vec<(u32, KeyPair)>,
    /// Sent are the stored keys in this range; ids that do not exist are skipped, as Baileys'
    /// `getPreKeys` skips them.
    pub upload: Range<u32>,
    pub next_pre_key_id: u32,
    pub first_unuploaded_pre_key_id: u32,
}

/// Baileys' `generateOrGetPreKeys` and `getNextPreKeys`: reuse keys generated but never
/// uploaded, generate only the shortfall, and advance both counters past what was sent.
pub fn plan(
    next_pre_key_id: u32,
    first_unuploaded_pre_key_id: u32,
    count: u32,
    mut generate: impl FnMut() -> KeyPair,
) -> Plan {
    let available = i64::from(next_pre_key_id) - i64::from(first_unuploaded_pre_key_id);
    let remaining = i64::from(count) - available;
    let last = i64::from(next_pre_key_id) + remaining - 1;
    let new_keys = if remaining > 0 {
        (next_pre_key_id..=last as u32)
            .map(|id| (id, generate()))
            .collect()
    } else {
        Vec::new()
    };
    let after_last = (last + 1).max(0) as u32;
    Plan {
        new_keys,
        upload: first_unuploaded_pre_key_id..first_unuploaded_pre_key_id + count,
        next_pre_key_id: after_last.max(next_pre_key_id),
        first_unuploaded_pre_key_id: first_unuploaded_pre_key_id.max(after_last),
    }
}

/// Baileys' `getNextPreKeysNode`, without the id its `query` appends.
pub fn upload_node(
    registration_id: u32,
    identity: &PublicKey,
    keys: &[(u32, &KeyPair)],
    signed: &SignedPreKey,
) -> Node {
    let list = Node {
        content: Content::Nodes(
            keys.iter()
                .map(|(id, key)| pre_key_node(*id, key))
                .collect(),
        ),
        ..Node::new("list")
    };
    Node {
        attrs: vec![
            ("xmlns".into(), Value::Str("encrypt".into())),
            ("type".into(), Value::Str("set".into())),
            ("to".into(), server()),
        ],
        content: Content::Nodes(vec![
            bytes("registration", registration_id.to_be_bytes().to_vec()),
            bytes("type", vec![5]),
            bytes("identity", identity.bytes().to_vec()),
            list,
            signed_pre_key_node(signed),
        ]),
        ..Node::new("iq")
    }
}

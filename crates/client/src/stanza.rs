//! What this client answers to what it receives: acks, receipts, retry receipts.
//!
//! Each is WAHA's Baileys' behaviour for the same stanza, ported so the server sees the client
//! it already knows: `Utils/stanza-ack.js` (`buildAckStanza`), `Utils/decode-wa-message.js`
//! (`decodeMessageNode`), `Socket/messages-send.js` (`sendReceipt`) and
//! `Socket/messages-recv.js` (`handleMessage`, `sendRetryRequest`). `tests/stanza.rs` checks
//! the bytes against Baileys run on the same stanzas.
//!
//! Baileys holds every JID as a string, which loses one thing the wire has: whether a JID was
//! sent with an explicit device 0. So a JID copied from a received stanza into a reply is
//! [`normalized`] first, exactly as the round trip through a Baileys string normalizes it.

use prost::Message;

use crate::baileys::{Account, SignedPreKey};
use crate::binary::{Content, Jid, JidForm, Node, Value};
use crate::signal::curve::{KeyPair, PublicKey};

/// Baileys' `NACK_REASONS.UnhandledError`, what a failed decryption is nacked with.
pub const NACK_UNHANDLED_ERROR: u16 = 500;

/// The server's own address, Baileys' `S_WHATSAPP_NET`: `@s.whatsapp.net`, a JID with no user.
pub fn server() -> Value {
    Value::Jid(Jid::pair("", "s.whatsapp.net"))
}

/// A JID the way Baileys writes one back: device-qualified only when the device is not 0.
pub fn normalized(jid: &Jid) -> Jid {
    match (jid.form, jid.device_number()) {
        (JidForm::Ad { .. }, 0) => Jid::pair(jid.user.clone(), jid.server.clone()),
        _ => jid.clone(),
    }
}

/// Baileys' `jidDecode`, for the account ids it keeps as strings: `user[_agent][:device]@server`.
pub fn parse_jid(text: &str) -> Option<Jid> {
    let (user_part, server) = text.split_once('@')?;
    let (user_agent, device) = match user_part.split_once(':') {
        Some((ua, device)) => (ua, Some(device.parse::<u8>().ok()?)),
        None => (user_part, None),
    };
    let user = user_agent.split('_').next().unwrap_or(user_agent);
    Some(match device {
        Some(device) => {
            Jid::device(user, server, device).unwrap_or_else(|| Jid::pair(user, server))
        }
        None => Jid::pair(user, server),
    })
}

fn copy(value: &Value) -> Value {
    match value {
        Value::Jid(jid) => Value::Jid(normalized(jid)),
        other => other.clone(),
    }
}

/// Baileys' `buildAckStanza`: the stanza acknowledging `node`, with an optional error code.
pub fn ack(node: &Node, error: Option<u16>, me: &Jid) -> Node {
    let mut ack = Node::new("ack");
    if let Some(id) = node.get("id") {
        ack.attrs.push(("id".into(), copy(id)));
    }
    if let Some(from) = node.get("from") {
        ack.attrs.push(("to".into(), copy(from)));
    }
    ack.attrs
        .push(("class".into(), Value::Str(node.tag.clone())));
    if let Some(code) = error {
        ack.attrs
            .push(("error".into(), Value::Str(code.to_string())));
    }
    for key in ["participant", "recipient", "type"] {
        if let Some(v) = node.get(key) {
            ack.attrs.push((key.into(), copy(v)));
        }
    }
    if node.tag == "message" {
        ack.attrs.push(("from".into(), Value::Jid(me.clone())));
    }
    ack
}

/// Who a message stanza is from and which chat it belongs to, as Baileys' `decodeMessageNode`
/// works it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageInfo {
    pub id: String,
    pub chat: Jid,
    pub from_me: bool,
    pub participant: Option<Jid>,
    /// The device that encrypted it: whose session or sender key decrypts it.
    pub author: Jid,
    pub category: Option<String>,
    pub kind: MessageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Chat,
    Group,
    Broadcast,
    Newsletter,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StanzaError {
    #[error("message stanza without {0}")]
    Missing(&'static str),
    #[error("recipient present, but the message is not from this account")]
    RecipientNotFromMe,
    #[error("unknown message type")]
    UnknownType,
}

/// `me` and `me_lid` are this account's two identities, phone number and LID.
pub fn message_info(
    node: &Node,
    me: &Jid,
    me_lid: Option<&Jid>,
) -> Result<MessageInfo, StanzaError> {
    let jid = |key| match node.get(key) {
        Some(Value::Jid(j)) => Some(normalized(j)),
        Some(Value::Str(s)) => parse_jid(s),
        None => None,
    };
    let id = node
        .get("id")
        .map(ToString::to_string)
        .ok_or(StanzaError::Missing("id"))?;
    let from = jid("from").ok_or(StanzaError::Missing("from"))?;
    let participant = jid("participant");
    let is_me = |j: &Jid| j.user == me.user || me_lid.is_some_and(|lid| j.user == lid.user);
    let category = node.get("category").map(ToString::to_string);
    let user_server = ["s.whatsapp.net", "lid", "hosted", "hosted.lid"];
    let (chat, author, from_me, kind) = if user_server.contains(&from.server.as_str()) {
        // A recipient on Meta AI's server does not make the message ours (`isJidMetaAI`).
        let recipient = jid("recipient").filter(|r| r.server != "bot");
        let from_me = is_me(&from);
        match recipient {
            Some(_) if !from_me => return Err(StanzaError::RecipientNotFromMe),
            Some(recipient) => (recipient, from.clone(), true, MessageKind::Chat),
            None => (from.clone(), from.clone(), from_me, MessageKind::Chat),
        }
    } else if from.server == "g.us" {
        let p = participant
            .clone()
            .ok_or(StanzaError::Missing("participant"))?;
        (from.clone(), p.clone(), is_me(&p), MessageKind::Group)
    } else if from.server == "broadcast" {
        let p = participant
            .clone()
            .ok_or(StanzaError::Missing("participant"))?;
        // Baileys checks only the phone-number identity here.
        let from_me = p.user == me.user;
        (from.clone(), p, from_me, MessageKind::Broadcast)
    } else if from.server == "newsletter" {
        (
            from.clone(),
            from.clone(),
            is_me(&from),
            MessageKind::Newsletter,
        )
    } else {
        return Err(StanzaError::UnknownType);
    };
    Ok(MessageInfo {
        id,
        chat,
        from_me,
        participant,
        author,
        category,
        kind,
    })
}

/// Baileys' `sendReceipt`: one receipt covering `ids`, the first as its id, the rest listed.
pub fn receipt(chat: &Jid, participant: Option<&Jid>, ids: &[String], kind: Option<&str>) -> Node {
    let mut node = Node::new("receipt").attr("id", ids[0].as_str());
    let is_user = chat.server == "s.whatsapp.net" || chat.server == "lid";
    if kind == Some("sender") && is_user {
        node.attrs
            .push(("recipient".into(), Value::Jid(chat.clone())));
        if let Some(p) = participant {
            node.attrs.push(("to".into(), Value::Jid(p.clone())));
        }
    } else {
        node.attrs.push(("to".into(), Value::Jid(chat.clone())));
        if let Some(p) = participant {
            node.attrs
                .push(("participant".into(), Value::Jid(p.clone())));
        }
    }
    if let Some(kind) = kind {
        node.attrs
            .push(("type".into(), Value::Str(kind.to_owned())));
    }
    if ids.len() > 1 {
        let items = ids[1..]
            .iter()
            .map(|id| Node::new("item").attr("id", id.as_str()))
            .collect();
        node.content = Content::Nodes(vec![Node {
            content: Content::Nodes(items),
            ..Node::new("list")
        }]);
    }
    node
}

/// What `handleMessage` sends once a message decrypted: a receipt of the right type, or for a
/// newsletter, which takes no receipts, an ack.
pub fn after_decrypt(node: &Node, info: &MessageInfo, me: &Jid) -> Node {
    if info.kind == MessageKind::Newsletter {
        return ack(node, None, me);
    }
    let (kind, participant) = if info.category.as_deref() == Some("peer") {
        ("peer_msg", info.participant.clone())
    } else if info.from_me {
        // shortcut: Baileys also switches to the author when the chat's *alternate* JID (from
        // the stanza's addressing attributes) is a LID; only the chat itself is checked here.
        let participant = if info.chat.server == "lid" {
            Some(info.author.clone())
        } else {
            info.participant.clone()
        };
        ("sender", participant)
    } else {
        // WAHA never sets `sendActiveReceipts`: delivered, but not as an active client.
        ("inactive", info.participant.clone())
    };
    receipt(
        &info.chat,
        participant.as_ref(),
        std::slice::from_ref(&info.id),
        Some(kind),
    )
}

/// The keys a retry receipt carries from its second attempt on, so the sender can start a fresh
/// session if ours is broken.
pub struct RetryKeys<'a> {
    pub identity: &'a PublicKey,
    pub pre_key: (u32, &'a KeyPair),
    pub signed_pre_key: &'a SignedPreKey,
    pub account: &'a Account,
}

/// Baileys' retry receipt: asks the sender to encrypt `node`'s message again.
pub fn retry_receipt(
    node: &Node,
    count: u32,
    registration_id: u32,
    keys: Option<RetryKeys<'_>>,
) -> Node {
    let get = |key| node.get(key).map(copy);
    let mut receipt = Node::new("receipt");
    if let Some(id) = get("id") {
        receipt.attrs.push(("id".into(), id));
    }
    receipt
        .attrs
        .push(("type".into(), Value::Str("retry".into())));
    if let Some(from) = get("from") {
        receipt.attrs.push(("to".into(), from));
    }
    for key in ["recipient", "participant"] {
        if let Some(v) = get(key) {
            receipt.attrs.push((key.into(), v));
        }
    }
    let mut retry = Node::new("retry").attr("count", count.to_string());
    for key in ["id", "t"] {
        if let Some(v) = get(key) {
            retry.attrs.push((key.into(), v));
        }
    }
    retry = retry.attr("v", "1").attr("error", "0");
    let mut content = vec![
        retry,
        bytes("registration", registration_id.to_be_bytes().to_vec()),
    ];
    if let Some(keys) = keys {
        content.push(Node {
            content: Content::Nodes(vec![
                bytes("type", vec![5]),
                bytes("identity", keys.identity.bytes().to_vec()),
                pre_key_node(keys.pre_key.0, keys.pre_key.1),
                signed_pre_key_node(keys.signed_pre_key),
                bytes("device-identity", signed_device_identity(keys.account)),
            ]),
            ..Node::new("keys")
        });
    }
    receipt.content = Content::Nodes(content);
    receipt
}

/// Baileys' `xmppPreKey`: id as 3 big-endian bytes, then the bare public key.
pub fn pre_key_node(id: u32, key: &KeyPair) -> Node {
    Node {
        content: Content::Nodes(vec![
            bytes("id", id.to_be_bytes()[1..].to_vec()),
            bytes("value", key.public.bytes().to_vec()),
        ]),
        ..Node::new("key")
    }
}

/// Baileys' `xmppSignedPreKey`.
pub fn signed_pre_key_node(key: &SignedPreKey) -> Node {
    Node {
        content: Content::Nodes(vec![
            bytes("id", key.id.to_be_bytes()[1..].to_vec()),
            bytes("value", key.key_pair.public.bytes().to_vec()),
            bytes("signature", key.signature.clone()),
        ]),
        ..Node::new("skey")
    }
}

/// `ADVSignedDeviceIdentity` with the account signature key included, as a retry sends it.
fn signed_device_identity(account: &Account) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct AdvSignedDeviceIdentity {
        #[prost(bytes = "vec", optional, tag = "1")]
        details: Option<Vec<u8>>,
        #[prost(bytes = "vec", optional, tag = "2")]
        account_signature_key: Option<Vec<u8>>,
        #[prost(bytes = "vec", optional, tag = "3")]
        account_signature: Option<Vec<u8>>,
        #[prost(bytes = "vec", optional, tag = "4")]
        device_signature: Option<Vec<u8>>,
    }
    AdvSignedDeviceIdentity {
        details: Some(account.details.clone()),
        account_signature_key: Some(account.account_signature_key.clone())
            .filter(|k| !k.is_empty()),
        account_signature: Some(account.account_signature.clone()),
        device_signature: Some(account.device_signature.clone()),
    }
    .encode_to_vec()
}

pub fn bytes(tag: &str, content: Vec<u8>) -> Node {
    Node {
        content: Content::Bytes(content),
        ..Node::new(tag)
    }
}

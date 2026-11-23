//! Replies against WAHA's own Baileys.
//!
//! `fixtures/stanza.json` comes from `scripts/baileys-stanza-fixture.mjs`, which ran Baileys'
//! `buildAckStanza`, `decodeMessageNode`, `sendReceipt` and `getNextPreKeysNode` on the same
//! inputs. The replies must be the same bytes.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use whatsapp_client::baileys::SignedPreKey;
use whatsapp_client::binary;
use whatsapp_client::prekeys;
use whatsapp_client::signal::curve::{KeyPair, PrivateKey, PublicKey};
use whatsapp_client::stanza::{self, parse_jid};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    me_id: String,
    me_lid: String,
    acks: Vec<Ack>,
    decoded: Vec<Decoded>,
    receipts: Vec<Receipt>,
    pre_key_upload: Upload,
}

#[derive(Deserialize)]
struct Ack {
    name: String,
    error: Option<u16>,
    input: String,
    output: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Decoded {
    name: String,
    input: String,
    chat: Option<String>,
    from_me: Option<bool>,
    participant: Option<String>,
    author: Option<String>,
    category: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Receipt {
    name: String,
    chat: String,
    participant: Option<String>,
    ids: Vec<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    output: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Upload {
    registration_id: u32,
    identity_public: String,
    signed_pre_key: Key,
    pre_keys: BTreeMap<u32, Key>,
    next_pre_key_id: u32,
    first_unuploaded_pre_key_id: u32,
    count: u32,
    output: String,
    update: Update,
}

#[derive(Deserialize)]
struct Key {
    private: String,
    signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Update {
    next_pre_key_id: u32,
    first_unuploaded_pre_key_id: u32,
}

fn fixture() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stanza.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Baileys' encoder writes the flags byte first; so does `marshal`.
fn node(b64: &str) -> binary::Node {
    binary::unmarshal(&B64.decode(b64).unwrap()).unwrap()
}

fn key_pair(b64: &str) -> KeyPair {
    let private = PrivateKey::deserialize(&B64.decode(b64).unwrap()).unwrap();
    KeyPair {
        public: private.public_key(),
        private,
    }
}

#[test]
fn acks_are_baileys_acks_byte_for_byte() {
    let f = fixture();
    let me = parse_jid(&f.me_id).unwrap();
    assert_eq!(f.acks.len(), 28);
    for case in &f.acks {
        let ack = stanza::ack(&node(&case.input), case.error, &me);
        assert_eq!(
            B64.encode(binary::marshal(&ack)),
            case.output,
            "{} (error {:?})",
            case.name,
            case.error
        );
    }
}

#[test]
fn messages_are_attributed_as_baileys_attributes_them() {
    let f = fixture();
    let (me, me_lid) = (parse_jid(&f.me_id).unwrap(), parse_jid(&f.me_lid).unwrap());
    for case in &f.decoded {
        let result = stanza::message_info(&node(&case.input), &me, Some(&me_lid));
        if let Some(error) = &case.error {
            assert!(
                result.is_err(),
                "{}: Baileys refused it ({error})",
                case.name
            );
            continue;
        }
        let info = result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(
            Some(info.chat.to_string()),
            case.chat,
            "{}: chat",
            case.name
        );
        assert_eq!(Some(info.from_me), case.from_me, "{}: fromMe", case.name);
        assert_eq!(
            info.participant.map(|p| p.to_string()),
            case.participant,
            "{}: participant",
            case.name
        );
        assert_eq!(
            Some(info.author.to_string()),
            case.author,
            "{}: author",
            case.name
        );
        assert_eq!(info.category, case.category, "{}: category", case.name);
    }
}

#[test]
fn receipts_are_baileys_receipts_byte_for_byte() {
    let f = fixture();
    for case in &f.receipts {
        let chat = parse_jid(&case.chat).unwrap();
        let participant = case.participant.as_deref().map(|p| parse_jid(p).unwrap());
        let receipt = stanza::receipt(&chat, participant.as_ref(), &case.ids, case.kind.as_deref());
        assert_eq!(
            B64.encode(binary::marshal(&receipt)),
            case.output,
            "{}",
            case.name
        );
    }
}

#[test]
fn the_pre_key_upload_is_baileys_upload_byte_for_byte() {
    let u = fixture().pre_key_upload;
    let plan = prekeys::plan(
        u.next_pre_key_id,
        u.first_unuploaded_pre_key_id,
        u.count,
        || unreachable!("all keys exist"),
    );
    assert!(plan.new_keys.is_empty());
    assert_eq!(
        (plan.next_pre_key_id, plan.first_unuploaded_pre_key_id),
        (
            u.update.next_pre_key_id,
            u.update.first_unuploaded_pre_key_id
        )
    );
    let stored: BTreeMap<u32, KeyPair> = u
        .pre_keys
        .iter()
        .map(|(id, k)| (*id, key_pair(&k.private)))
        .collect();
    let keys: Vec<(u32, &KeyPair)> = plan
        .upload
        .clone()
        .filter_map(|id| stored.get(&id).map(|k| (id, k)))
        .collect();
    let signed = SignedPreKey {
        id: 7,
        key_pair: key_pair(&u.signed_pre_key.private),
        signature: B64
            .decode(u.signed_pre_key.signature.as_ref().unwrap())
            .unwrap(),
    };
    let identity = PublicKey::from_djb(&B64.decode(&u.identity_public).unwrap()).unwrap();
    let upload = prekeys::upload_node(u.registration_id, &identity, &keys, &signed);
    assert_eq!(B64.encode(binary::marshal(&upload)), u.output);
}

#[test]
fn a_shortfall_is_generated_and_the_counters_move_past_it() {
    // Nothing generated-but-unsent: the whole count is new, ids continuing from `next`.
    let mut made = 0;
    let plan = prekeys::plan(100, 100, 5, || {
        made += 1;
        KeyPair::generate()
    });
    assert_eq!(made, 5);
    assert_eq!(
        plan.new_keys.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [100, 101, 102, 103, 104]
    );
    assert_eq!(plan.upload, 100..105);
    assert_eq!(
        (plan.next_pre_key_id, plan.first_unuploaded_pre_key_id),
        (105, 105)
    );
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WahaIds {
    name: String,
    input: String,
    waha_message_id: Option<String>,
    waha_author: Option<String>,
}

#[test]
fn message_ids_and_authors_are_wahas() {
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stanza.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let (me, me_lid) = (
        parse_jid(raw["meId"].as_str().unwrap()).unwrap(),
        parse_jid(raw["meLid"].as_str().unwrap()).unwrap(),
    );
    let cases: Vec<WahaIds> = serde_json::from_value(raw["decoded"].clone()).unwrap();
    let mut checked = 0;
    for case in cases {
        let Some(expected_id) = case.waha_message_id else {
            continue;
        };
        let info = stanza::message_info(&node(&case.input), &me, Some(&me_lid)).unwrap();
        assert_eq!(
            whatsapp_client::rows::waha_message_id(&info),
            expected_id,
            "{}: message id",
            case.name
        );
        assert_eq!(
            whatsapp_client::rows::waha_author(&info),
            case.waha_author,
            "{}: author",
            case.name
        );
        checked += 1;
    }
    assert_eq!(checked, 10);
}

#[derive(Deserialize)]
struct TargetCase {
    name: String,
    kind: String,
    input: String,
    key: serde_json::Value,
    expected: String,
}

#[test]
fn reaction_and_edit_targets_are_the_ids_waha_files_them_under() {
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stanza.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let (me, me_lid) = (
        parse_jid(raw["meId"].as_str().unwrap()).unwrap(),
        parse_jid(raw["meLid"].as_str().unwrap()).unwrap(),
    );
    let cases: Vec<TargetCase> = serde_json::from_value(raw["targets"].clone()).unwrap();
    assert_eq!(cases.len(), 6);
    for case in cases {
        let info = stanza::message_info(&node(&case.input), &me, Some(&me_lid)).unwrap();
        let target = whatsapp_client::content::Target {
            remote_jid: case.key["remoteJid"].as_str().map(str::to_owned),
            from_me: case.key["fromMe"].as_bool().unwrap_or(false),
            id: case.key["id"].as_str().unwrap().to_owned(),
            participant: case.key["participant"].as_str().map(str::to_owned),
        };
        let id = match case.kind.as_str() {
            "reaction" => {
                whatsapp_client::rows::reaction_target_id(&info, &target, &me, Some(&me_lid))
            }
            _ => whatsapp_client::rows::edit_target_id(&info, &target.id),
        };
        assert_eq!(id, case.expected, "{}", case.name);
    }
}

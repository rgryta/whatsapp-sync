//! The import is proven by decryption, not by inspection.
//!
//! `fixtures/` was made by `scripts/baileys-fixture.mjs` running on WAHA's own Baileys: a
//! session for a device "alice", then messages to alice that Baileys has not decrypted. If
//! any byte of the import were wrong (a counter off by one, a chain in the wrong role, a key
//! in the wrong encoding) these would not decrypt, or a replay would.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use whatsapp_client::baileys;
use whatsapp_client::group::GroupError;
use whatsapp_client::signal::{Address, SignalError};

/// A decrypted fixture message's text: unpadded and decoded as WhatsApp's `Message`.
fn text(plaintext: &[u8]) -> String {
    let message =
        whatsapp_client::content::decode(whatsapp_client::content::unpad(plaintext).unwrap())
            .unwrap();
    message.conversation.unwrap()
}

#[derive(Deserialize)]
struct Pending {
    kind: String,
    name: String,
    sender: String,
    #[serde(rename = "type")]
    message_type: Option<String>,
    group: Option<String>,
    ciphertext: String,
    expect: String,
}

fn fixtures() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
}

fn pending() -> Vec<Pending> {
    serde_json::from_slice(&std::fs::read(fixtures().join("messages.json")).unwrap()).unwrap()
}

/// A sender as Baileys addresses it: `<user>@lid` is LID domain 1, device 0.
fn address(jid: &str) -> Address {
    let user = jid.strip_suffix("@lid").expect("fixture senders are LIDs");
    Address {
        name: format!("{user}_1"),
        device: 0,
    }
}

fn bob() -> Address {
    address("111111111111111@lid")
}

#[test]
fn the_device_imports_with_every_key_checked() {
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    assert!(device.account.is_some());
    assert_eq!(
        device.me.as_ref().unwrap().lid.as_deref(),
        Some("222222222222222:3@lid")
    );
    // Five were made and Bob's first message used up number 2, as WhatsApp's first contact does.
    let mut ids: Vec<u32> = device.pre_keys.iter().map(|(id, _)| *id).collect();
    ids.sort();
    assert_eq!(ids, [1, 3, 4, 5]);
    assert_eq!(device.sessions.len(), 1);
    assert_eq!(device.stats.current_sessions, 1);
    assert_eq!(device.identities.len(), 1);
    assert_eq!(device.sender_keys.len(), 1);
}

#[test]
fn pairwise_messages_baileys_never_read_decrypt_after_the_import() {
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    let mut store = device.signal_store();
    let carol = address("333333333333333@lid");
    assert!(
        !store.sessions.contains_key(&carol),
        "carol's session must start in Rust"
    );

    let mut checked = 0;
    for message in pending().into_iter().filter(|m| m.kind == "pairwise") {
        let bytes = B64.decode(&message.ciphertext).unwrap();
        let from = address(&message.sender);
        let result = match message.message_type.as_deref() {
            Some("pkmsg") => store.decrypt_prekey(&from, &bytes),
            Some("msg") => store.decrypt(&from, &bytes),
            other => panic!("{}: type {other:?}", message.name),
        };
        match message.expect.as_str() {
            "plaintext" => assert_eq!(text(&result.unwrap()), message.name, "{}", message.name),
            "duplicate" => assert!(
                matches!(result, Err(SignalError::Duplicate { .. })),
                "{} must be refused as already read, got {result:?}",
                message.name
            ),
            other => panic!("expect {other}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 9);
    // Carol's first message used pre-key 3; X3DH consumes a one-time pre-key exactly once.
    assert!(!store.pre_keys.contains_key(&3));
    assert!(store.sessions[&carol].has_current_session());
}

#[test]
fn a_message_that_does_not_authenticate_is_refused_and_changes_nothing() {
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    let mut store = device.signal_store();
    let message = pending().into_iter().find(|m| m.name == "m7").unwrap();
    let mut bytes = B64.decode(&message.ciphertext).unwrap();
    let before = store.sessions[&bob()].serialize();
    let last = bytes.len() - 1;
    bytes[last] ^= 1; // the MAC
    assert!(matches!(
        store.decrypt(&bob(), &bytes),
        Err(SignalError::InvalidMessage(_))
    ));
    assert_eq!(store.sessions[&bob()].serialize(), before);
    bytes[last] ^= 1;
    assert_eq!(text(&store.decrypt(&bob(), &bytes).unwrap()), "m7");
}

#[test]
fn group_messages_baileys_never_read_decrypt_after_the_import() {
    let mut device = baileys::import(&fixtures().join("alice")).unwrap();
    let mut checked = 0;
    for message in pending().into_iter().filter(|m| m.kind == "group") {
        let key = (message.group.clone().unwrap(), bob());
        let record = device
            .sender_keys
            .get_mut(&key)
            .expect("bob's sender key for the group");
        let result = record.decrypt(&B64.decode(&message.ciphertext).unwrap());
        match message.expect.as_str() {
            "plaintext" => assert_eq!(text(&result.unwrap()), message.name, "{}", message.name),
            "duplicate" => assert!(
                matches!(result, Err(GroupError::Duplicate(_))),
                "{}: {result:?}",
                message.name
            ),
            other => panic!("expect {other}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn a_tampered_group_message_is_refused_and_changes_nothing() {
    let mut device = baileys::import(&fixtures().join("alice")).unwrap();
    let message = pending().into_iter().find(|m| m.name == "g4").unwrap();
    let mut bytes = B64.decode(&message.ciphertext).unwrap();
    let record = device
        .sender_keys
        .get_mut(&(message.group.unwrap(), bob()))
        .unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    assert_eq!(record.decrypt(&bytes).err(), Some(GroupError::Signature));
    bytes[middle] ^= 1;
    assert_eq!(text(&record.decrypt(&bytes).unwrap()), "g4");
}

#[test]
fn a_group_message_that_arrives_after_a_later_one_still_decrypts() {
    // g2 was skipped inside Baileys, so its key came with the import. Reading g5 before g4
    // makes this crate skip g4 itself and keep its key, which is the path to prove here.
    let mut device = baileys::import(&fixtures().join("alice")).unwrap();
    let messages = pending();
    let ciphertext = |name: &str| {
        B64.decode(&messages.iter().find(|m| m.name == name).unwrap().ciphertext)
            .unwrap()
    };
    let group = messages
        .iter()
        .find(|m| m.kind == "group")
        .unwrap()
        .group
        .clone()
        .unwrap();
    let record = device.sender_keys.get_mut(&(group, bob())).unwrap();
    assert_eq!(text(&record.decrypt(&ciphertext("g5")).unwrap()), "g5");
    assert_eq!(text(&record.decrypt(&ciphertext("g4")).unwrap()), "g4");
    assert!(matches!(
        record.decrypt(&ciphertext("g4")),
        Err(GroupError::Duplicate(_))
    ));
}

#[test]
fn a_pairwise_message_that_arrives_after_a_later_one_still_decrypts() {
    // m7 and m8 open a chain alice had not seen. Reading m8 first makes this crate take the
    // ratchet step and keep m7's key itself.
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    let mut store = device.signal_store();
    let messages = pending();
    let ciphertext = |name: &str| {
        B64.decode(&messages.iter().find(|m| m.name == name).unwrap().ciphertext)
            .unwrap()
    };
    assert_eq!(
        text(&store.decrypt(&bob(), &ciphertext("m8")).unwrap()),
        "m8"
    );
    assert_eq!(
        text(&store.decrypt(&bob(), &ciphertext("m7")).unwrap()),
        "m7"
    );
    assert!(matches!(
        store.decrypt(&bob(), &ciphertext("m7")),
        Err(SignalError::Duplicate { .. })
    ));
}

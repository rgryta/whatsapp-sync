//! Edits sealed under the original message's secret, against `fixtures/secret_edit.json`
//! (`scripts/secret-edit-fixture.mjs`: Node's crypto and WAHA's protobuf definitions).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::json;
use whatsapp_client::content::{self, Event};
use whatsapp_client::secret;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    secret: String,
    original_id: String,
    original_sender: String,
    editor: String,
    original: String,
    edit: String,
    iv: String,
    payload: String,
    body: String,
}

fn fixture() -> Fixture {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/secret_edit.json"
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn b(s: &str) -> Vec<u8> {
    B64.decode(s).unwrap()
}

#[test]
fn the_original_keeps_its_secret_from_the_outer_envelope() {
    let f = fixture();
    let message = content::decode(&b(&f.original)).unwrap();
    match content::event(&message) {
        Event::Message(columns) => {
            assert_eq!(columns.body.as_deref(), Some("teh original"));
            assert_eq!(columns.secret, Some(b(&f.secret)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_edit_is_recognised_and_opens_to_the_new_text() {
    let f = fixture();
    let edit = content::decode(&b(&f.edit)).unwrap();
    let Event::SecretEdit {
        target,
        iv,
        payload,
    } = content::event(&edit)
    else {
        panic!("not a secret edit");
    };
    assert_eq!(target.id, f.original_id);
    assert!(target.from_me);
    assert_eq!((iv.clone(), payload.clone()), (b(&f.iv), b(&f.payload)));

    let plain = secret::decrypt(
        &b(&f.secret),
        &f.original_id,
        &f.original_sender,
        &f.editor,
        &iv,
        &payload,
    )
    .expect("opens under the right senders");
    match content::decode(&plain).map(|m| content::event(&m)) {
        Some(Event::Edit { target, body }) => {
            assert_eq!(target.id, f.original_id);
            assert_eq!(body.as_deref(), Some(f.body.as_str()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_the_right_senders_secret_and_id_open_it() {
    let f = fixture();
    let (secret_bytes, iv, payload) = (b(&f.secret), b(&f.iv), b(&f.payload));
    let open = |originals: &[&str], editors: &[&str]| {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        secret::open(
            &secret_bytes,
            &f.original_id,
            &own(originals),
            &own(editors),
            &iv,
            &payload,
        )
    };
    let wrong = "999@lid";
    // The right pair found among wrong candidates, in either list.
    assert!(open(&[wrong, &f.original_sender], &[wrong, &f.editor]).is_some());
    // Swapped roles, a wrong sender, or no candidates: nothing opens.
    assert!(open(&[&f.editor], &[&f.original_sender]).is_none());
    assert!(open(&[wrong], &[&f.editor]).is_none());
    assert!(open(&[], &[&f.editor]).is_none());
    let decrypt = |secret: &[u8], id: &str, iv: &[u8]| {
        secret::decrypt(secret, id, &f.original_sender, &f.editor, iv, &payload)
    };
    assert!(decrypt(&[0; 32], &f.original_id, &iv).is_none());
    assert!(decrypt(&secret_bytes, "OTHER", &iv).is_none());
    assert!(decrypt(&secret_bytes, &f.original_id, &iv[..11]).is_none());
}

#[test]
fn the_secret_is_read_from_our_rows_and_from_wahas() {
    let f = fixture();
    let bytes = b(&f.secret);
    assert_eq!(
        secret::from_raw(&json!({"messageSecret": f.secret})),
        Some(bytes.clone())
    );
    let waha = json!({"_data": {"message": {"messageContextInfo": {
        "messageSecret": {"type": "Buffer", "data": f.secret}
    }}}});
    assert_eq!(secret::from_raw(&waha), Some(bytes));
    assert_eq!(secret::from_raw(&json!({"messageSecret": null})), None);
    assert_eq!(secret::from_raw(&json!({})), None);
}

#[test]
fn a_message_kept_nothing_of_is_named_by_its_field_numbers() {
    let f = fixture();
    assert_eq!(content::field_numbers(&b(&f.edit)), [35, 82]);
    assert_eq!(content::field_numbers(&b(&f.original)), [31, 35]);
    // A truncated message stops at the field it cannot skip, rather than reading past its end.
    let edit = b(&f.edit);
    assert_eq!(content::field_numbers(&edit[..edit.len() - 1]), [35, 82]);
    assert_eq!(content::field_numbers(&[0xff]), Vec::<u64>::new());
    // Fixed-width fields are skipped by their width: 64-bit (1), varint (2), 32-bit (3), varint (4).
    let fixed = [
        [0x09].as_slice(),
        &[0; 8],
        &[0x10, 0x01, 0x1d],
        &[0; 4],
        &[0x20, 0x00],
    ]
    .concat();
    assert_eq!(content::field_numbers(&fixed), [1, 2, 3, 4]);
}

#[test]
fn a_message_with_only_its_secret_is_not_content() {
    let only_secret = content::Message {
        message_context_info: Some(content::MessageContextInfo {
            message_secret: Some(vec![7; 32]),
        }),
        ..Default::default()
    };
    assert_eq!(content::event(&only_secret), Event::Other);
}

//! Message content against WAHA's own extraction.
//!
//! `fixtures/content.json` comes from `scripts/baileys-content-fixture.mjs`: each message's
//! columns as WAHA's `extractBody`, `extractMediaContent` and `extractReplyTo` and the Python
//! ingester's kind rule make them. A message must map to the same columns; an edit, deletion
//! or reaction must be recognised as one, because WAHA applies those to the original instead
//! of archiving them.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use whatsapp_client::content::{self, Columns, Event};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    bytes: String,
    body: Option<String>,
    kind: String,
    reply_to_id: Option<String>,
    has_media: bool,
    media_mime: Option<String>,
    media_filename: Option<String>,
    media_seconds: Option<i32>,
}

fn cases() -> Vec<Case> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/content.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn event(case: &Case) -> Event {
    content::event(&content::decode(&B64.decode(&case.bytes).unwrap()).expect(&case.name))
}

#[test]
fn messages_map_to_the_columns_waha_and_the_ingester_give_them() {
    let mut checked = 0;
    for case in cases() {
        let Event::Message(columns) = event(&case) else {
            continue;
        };
        let expected = Columns {
            body: case.body.clone(),
            kind: Box::leak(case.kind.clone().into_boxed_str()),
            reply_to_id: case.reply_to_id.clone(),
            has_media: case.has_media,
            media_mime: case.media_mime.clone(),
            media_filename: case.media_filename.clone(),
            media_seconds: case.media_seconds,
            audio: None,
            secret: None,
        };
        assert_eq!(columns, expected, "{}", case.name);
        checked += 1;
    }
    assert_eq!(checked, 20, "every ordinary message in the fixture");
}

#[test]
fn edits_deletions_and_reactions_are_events_on_the_original() {
    let cases = cases();
    let get = |name: &str| cases.iter().find(|c| c.name == name).unwrap();
    for name in ["edit", "edit, wrapped"] {
        let case = get(name);
        match event(case) {
            Event::Edit { target, body } => {
                assert_eq!(target.id, "3EB0TARGET");
                assert_eq!(body, case.body, "{name}: the new text, as WAHA reads it");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    assert!(matches!(event(get("revoke")), Event::Revoke { target } if target.id == "3EB0TARGET"));
    assert!(
        matches!(event(get("revoke, type left off")), Event::Revoke { target } if target.id == "3EB0TARGET")
    );
    assert!(
        matches!(event(get("reaction")), Event::Reaction { emoji, ts_ms: Some(1_700_000_000_123), .. } if emoji == "👍")
    );
    assert!(
        matches!(event(get("reaction removed")), Event::Reaction { emoji, .. } if emoji.is_empty())
    );
    assert_eq!(event(get("key distribution only")), Event::Other);
}

#[test]
fn padding_comes_off_as_baileys_strips_it() {
    assert_eq!(content::unpad(&[7, 8, 9, 2, 2]), Some(&[7, 8, 9][..]));
    assert_eq!(content::unpad(&[1]), Some(&[][..]));
    assert_eq!(
        content::unpad(&[1, 5]),
        None,
        "a pad longer than the message"
    );
    assert_eq!(content::unpad(&[]), None);
}

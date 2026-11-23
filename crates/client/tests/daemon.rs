//! The daemon end to end, without WhatsApp: real stanzas in, replies recorded, rows checked.
//!
//! The stanzas carry the fixture's ciphertexts (`scripts/baileys-fixture.mjs`, made by WAHA's
//! own Baileys), so everything from decryption to the archive row runs for real. Postgres is
//! the throwaway test container, a database of its own.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, MutexGuard, OnceCell};
use whatsapp_client::binary::{Content, Jid, Node, Value};
use whatsapp_client::conn::{ConnError, Outbox};
use whatsapp_client::daemon::Client;
use whatsapp_client::login::LoginSettings;
use whatsapp_client::{baileys, store};

static CONTAINER: OnceCell<Option<(ContainerAsync<Postgres>, String)>> = OnceCell::const_new();
static TURN: Mutex<()> = Mutex::const_new(());

async fn database() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let turn = TURN.lock().await;
    let server = CONTAINER
        .get_or_init(|| async {
            let request = Postgres::default()
                .with_tag("18-alpine")
                .with_container_name("whatsapp-sync-test-pg")
                .with_reuse(ReuseDirective::Always);
            match request.start().await {
                Ok(c) => {
                    let (host, port) = (
                        c.get_host().await.ok()?,
                        c.get_host_port_ipv4(5432).await.ok()?,
                    );
                    Some((c, format!("postgresql://postgres:postgres@{host}:{port}")))
                }
                Err(e) => {
                    eprintln!("SKIPPING the daemon tests: docker is not reachable: {e}");
                    None
                }
            }
        })
        .await
        .as_ref()
        .map(|(_, s)| s.clone())?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{server}/postgres"))
        .await
        .unwrap();
    sqlx::query("DROP DATABASE IF EXISTS daemon_test WITH (FORCE)")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE daemon_test")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{server}/daemon_test"))
        .await
        .unwrap();
    whatsapp_core::db::ensure_schema(&pool).await.unwrap();
    store::migrate(&pool).await.unwrap();
    Some((pool, turn))
}

#[derive(Default)]
struct Recorder {
    sent: Vec<Node>,
    tags: u32,
}

impl Outbox for Recorder {
    async fn send(&mut self, node: &Node) -> Result<(), ConnError> {
        self.sent.push(node.clone());
        Ok(())
    }

    fn next_tag(&mut self) -> String {
        self.tags += 1;
        format!("test-{}", self.tags)
    }
}

#[derive(Deserialize)]
struct Pending {
    kind: String,
    name: String,
    id: String,
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

fn lid(jid: &str) -> Jid {
    Jid::pair(jid.strip_suffix("@lid").unwrap(), "lid")
}

/// A `<message>` as the server delivers it.
fn stanza(m: &Pending, extra: &[(&str, Value)]) -> Node {
    let enc_type = if m.kind == "group" {
        "skmsg".to_owned()
    } else {
        m.message_type.clone().unwrap()
    };
    let enc = Node {
        content: Content::Bytes(B64.decode(&m.ciphertext).unwrap()),
        ..Node::new("enc").attr("v", "2").attr("type", enc_type)
    };
    let mut node = match &m.group {
        Some(group) => Node::new("message")
            .attr("from", Jid::pair(group.trim_end_matches("@g.us"), "g.us"))
            .attr("participant", lid(&m.sender)),
        None => Node::new("message").attr("from", lid(&m.sender)),
    };
    node = node
        .attr("id", m.id.as_str())
        .attr("t", "1700000000")
        .attr("type", "text")
        .attr("notify", "Bob");
    for (k, v) in extra {
        node.attrs.push(((*k).to_owned(), v.clone()));
    }
    node.content = Content::Nodes(vec![enc]);
    node
}

async fn bodies(pool: &PgPool) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT chat_id, message_id, body FROM message ORDER BY message_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn messages_are_archived_and_answered_and_the_state_is_kept() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    store::save(&pool, &device).await.unwrap();
    let mut client = Client::new(pool.clone(), device, LoginSettings::default());
    let mut out = Recorder::default();
    let pending: Vec<Pending> =
        serde_json::from_slice(&std::fs::read(fixtures().join("messages.json")).unwrap()).unwrap();
    let carol_pn = [(
        "sender_pn",
        Value::Jid(Jid::pair("48333000000", "s.whatsapp.net")),
    )];

    for m in &pending {
        let extra: &[(&str, Value)] = if m.sender.starts_with("333") {
            &carol_pn
        } else {
            &[]
        };
        let before = out.sent.len();
        client.handle(&mut out, stanza(m, extra)).await.unwrap();
        let reply = &out.sent[before..];
        assert_eq!(reply.len(), 1, "{}: one reply", m.name);
        // Decrypted or already decrypted, the answer is the delivery receipt WAHA sends.
        assert_eq!(reply[0].tag, "receipt", "{} ({})", m.name, m.expect);
        assert_eq!(
            reply[0].get("type").map(ToString::to_string).as_deref(),
            Some("inactive"),
            "{}",
            m.name
        );
        assert_eq!(
            reply[0].get("id").map(ToString::to_string).as_deref(),
            Some(m.id.as_str())
        );
    }

    let rows = bodies(&pool).await;
    let plaintexts = pending.iter().filter(|m| m.expect == "plaintext").count();
    assert_eq!(
        rows.len(),
        plaintexts,
        "one row per message, none for the replays"
    );
    for m in pending.iter().filter(|m| m.expect == "plaintext") {
        let row = rows
            .iter()
            .find(|(_, id, _)| id.ends_with(&m.id) || id.contains(&format!("_{}_", m.id)))
            .unwrap_or_else(|| panic!("{} archived", m.name));
        assert_eq!(row.2.as_deref(), Some(m.name.as_str()), "{}: body", m.name);
    }
    // Carol's stanzas named her number, so her chat is filed under it and the pair is kept.
    assert!(
        rows.iter()
            .any(|(chat, id, _)| chat == "48333000000@s.whatsapp.net"
                && id == "false_333333333333333@lid_P-c1")
    );
    let mapping: Option<String> =
        sqlx::query_scalar("SELECT lid FROM device.lid_mapping WHERE pn = '48333000000'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(mapping.as_deref(), Some("333333333333333"));
    // Carol's first message used up pre-key 3, durably, and her session was saved.
    let pre_keys: Vec<i32> = sqlx::query_scalar("SELECT id FROM device.pre_key ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(pre_keys, [1, 4, 5]);
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM device.session WHERE name = '333333333333333_1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sessions, 1);

    // The saved state is enough to carry on: a fresh client from the store refuses the replays.
    let reloaded = store::load(&pool).await.unwrap().unwrap();
    let mut client = Client::new(pool.clone(), reloaded, LoginSettings::default());
    let c2 = pending.iter().find(|m| m.name == "c2").unwrap();
    let before = bodies(&pool).await.len();
    client
        .handle(&mut out, stanza(c2, &carol_pn))
        .await
        .unwrap();
    assert_eq!(out.sent.last().unwrap().tag, "receipt");
    assert_eq!(
        bodies(&pool).await.len(),
        before,
        "a replay after a restart adds nothing"
    );
}

#[tokio::test]
async fn a_message_that_does_not_decrypt_gets_a_retry_receipt_and_a_nack() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    store::save(&pool, &device).await.unwrap();
    let mut client = Client::new(pool.clone(), device, LoginSettings::default());
    let mut out = Recorder::default();
    let pending: Vec<Pending> =
        serde_json::from_slice(&std::fs::read(fixtures().join("messages.json")).unwrap()).unwrap();
    let m7 = pending.iter().find(|m| m.name == "m7").unwrap();
    let mut broken = stanza(m7, &[]);
    if let Content::Nodes(children) = &mut broken.content
        && let Content::Bytes(bytes) = &mut children[0].content
    {
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
    }
    client.handle(&mut out, broken).await.unwrap();
    assert_eq!(
        out.sent.iter().map(|n| n.tag.as_str()).collect::<Vec<_>>(),
        ["receipt", "ack"]
    );
    assert_eq!(
        out.sent[0].get("type").map(ToString::to_string).as_deref(),
        Some("retry")
    );
    assert_eq!(
        out.sent[1].get("error").map(ToString::to_string).as_deref(),
        Some("500")
    );
    assert!(bodies(&pool).await.is_empty(), "nothing archived");
}

#[tokio::test]
async fn a_low_pre_key_notice_tops_the_server_up_and_saves_the_keys_first() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    let next = device.next_pre_key_id;
    store::save(&pool, &device).await.unwrap();
    let mut client = Client::new(pool.clone(), device, LoginSettings::default());
    let mut out = Recorder::default();
    let notice = Node {
        content: Content::Nodes(vec![Node::new("count").attr("value", "2")]),
        ..Node::new("notification")
            .attr("from", Jid::pair("", "s.whatsapp.net"))
            .attr("id", "N1")
            .attr("type", "encrypt")
            .attr("t", "1700000000")
    };
    client.handle(&mut out, notice).await.unwrap();
    let tags: Vec<&str> = out.sent.iter().map(|n| n.tag.as_str()).collect();
    assert_eq!(tags, ["iq", "ack"]);
    let keys = out.sent[0].child("list").unwrap().children().len();
    assert_eq!(keys, 5, "Baileys' top-up of MIN_PREKEY_COUNT");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM device.pre_key WHERE id >= $1")
        .bind(next as i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 5, "every uploaded key is stored");
    let counters: (i32, i32) =
        sqlx::query_as("SELECT next_pre_key_id, first_unuploaded_pre_key_id FROM device.account")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counters, (next as i32 + 5, next as i32 + 5));
}

#[tokio::test]
async fn a_read_receipt_raises_the_ack_on_our_message() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let device = baileys::import(&fixtures().join("alice")).unwrap();
    store::save(&pool, &device).await.unwrap();
    let mut client = Client::new(pool.clone(), device, LoginSettings::default());
    let mut out = Recorder::default();
    let chat = "111111111111111@lid";
    whatsapp_core::store::writes::ensure_chat(&pool, chat, "dm")
        .await
        .unwrap();
    let mine = whatsapp_core::models::MessageRow {
        chat_id: chat.into(),
        message_id: "true_111111111111111@lid_OURS".into(),
        ts: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        from_me: true,
        author_id: Some(chat.into()),
        body: Some("hi".into()),
        kind: "text".into(),
        reply_to_id: None,
        ack: Some(1),
        has_media: false,
        media_mime: None,
        media_filename: None,
        media_seconds: None,
        transcript_state: "n/a".into(),
        raw: serde_json::json!({}),
    };
    whatsapp_core::store::writes::upsert_message(&pool, &mine)
        .await
        .unwrap();
    let read = Node::new("receipt")
        .attr("from", lid(chat))
        .attr("id", "OURS")
        .attr("type", "read")
        .attr("t", "1700000001");
    client.handle(&mut out, read).await.unwrap();
    let ack: Option<i32> = sqlx::query_scalar("SELECT ack FROM message")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ack, Some(3));
    assert_eq!(out.sent.last().unwrap().tag, "ack");
}

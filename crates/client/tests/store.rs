//! The device store against a throwaway Postgres.
//!
//! The same named test container the archive tests use (see `tests/archive/support.rs`), but
//! a database of its own, so neither suite's truncation reaches the other. Never a real
//! database: the DSN is built from the container, nothing is read from the environment.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, MutexGuard, OnceCell};
use whatsapp_client::signal::{Address, SignalError};
use whatsapp_client::{baileys, store};

/// A decrypted fixture message's text: unpadded and decoded as WhatsApp's `Message`.
fn text(plaintext: &[u8]) -> String {
    let message =
        whatsapp_client::content::decode(whatsapp_client::content::unpad(plaintext).unwrap())
            .unwrap();
    message.conversation.unwrap()
}

const CONTAINER_NAME: &str = "whatsapp-sync-test-pg";
const DATABASE: &str = "client_test";

static CONTAINER: OnceCell<Option<(ContainerAsync<Postgres>, String)>> = OnceCell::const_new();
static TURN: Mutex<()> = Mutex::const_new(());

/// A pool on an emptied `client_test` database, with the device schema applied, or `None`
/// when Docker is unreachable (the tests then report a skip and return).
async fn database() -> Option<(PgPool, String, MutexGuard<'static, ()>)> {
    let turn = TURN.lock().await;
    let server = CONTAINER
        .get_or_init(|| async {
            let request = Postgres::default()
                .with_tag("18-alpine")
                .with_container_name(CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always);
            match request.start().await {
                Ok(container) => {
                    let host = container.get_host().await.ok()?;
                    let port = container.get_host_port_ipv4(5432).await.ok()?;
                    Some((
                        container,
                        format!("postgresql://postgres:postgres@{host}:{port}"),
                    ))
                }
                Err(error) => {
                    eprintln!("SKIPPING the device store tests: docker is not reachable: {error}");
                    None
                }
            }
        })
        .await
        .as_ref()
        .map(|(_, server)| server.clone())?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{server}/postgres"))
        .await
        .unwrap();
    sqlx::query("DROP DATABASE IF EXISTS client_test WITH (FORCE)")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE client_test")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!("{server}/{DATABASE}"))
        .await
        .unwrap();
    Some((pool, server, turn))
}

fn fixtures() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
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

fn address(jid: &str) -> Address {
    Address {
        name: format!("{}_1", jid.strip_suffix("@lid").unwrap()),
        device: 0,
    }
}

#[tokio::test]
async fn migrations_apply_once() {
    let Some((pool, _, _turn)) = database().await else {
        return;
    };
    assert_eq!(store::migrate(&pool).await.unwrap(), ["0001_device.sql"]);
    assert!(store::migrate(&pool).await.unwrap().is_empty());
    assert!(
        store::load(&pool).await.unwrap().is_none(),
        "nothing saved yet"
    );
}

#[tokio::test]
async fn a_saved_device_reloads_able_to_decrypt_everything_the_import_could() {
    let Some((pool, _, _turn)) = database().await else {
        return;
    };
    store::migrate(&pool).await.unwrap();
    let imported = baileys::import(&fixtures().join("alice")).unwrap();
    store::save(&pool, &imported).await.unwrap();
    let mut device = store::load(&pool).await.unwrap().expect("saved");

    assert_eq!(device.identity.public, imported.identity.public);
    assert_eq!(device.noise.public, imported.noise.public);
    assert_eq!(device.registration_id, imported.registration_id);
    assert_eq!(
        device.signed_pre_key.signature,
        imported.signed_pre_key.signature
    );
    assert_eq!(
        device.account.as_ref().unwrap().device_signature,
        imported.account.as_ref().unwrap().device_signature
    );
    assert_eq!(
        device.me.as_ref().unwrap().lid,
        imported.me.as_ref().unwrap().lid
    );
    assert_eq!(
        (
            device.pre_keys.len(),
            device.sessions.len(),
            device.identities.len()
        ),
        (4, 1, 1)
    );
    assert_eq!(device.sender_keys.len(), 1);

    // The proof is the same as the import's: every message Baileys left unread decrypts.
    let pending: Vec<Pending> =
        serde_json::from_slice(&std::fs::read(fixtures().join("messages.json")).unwrap()).unwrap();
    let mut signal = device.signal_store();
    for m in &pending {
        let bytes = B64.decode(&m.ciphertext).unwrap();
        let result = match (m.kind.as_str(), m.message_type.as_deref()) {
            ("pairwise", Some("pkmsg")) => signal.decrypt_prekey(&address(&m.sender), &bytes),
            ("pairwise", _) => signal.decrypt(&address(&m.sender), &bytes),
            _ => {
                let key = (m.group.clone().unwrap(), address(&m.sender));
                let record = device.sender_keys.get_mut(&key).unwrap();
                record
                    .decrypt(&bytes)
                    .map_err(|_| SignalError::InvalidMessage("group"))
            }
        };
        match m.expect.as_str() {
            "plaintext" => assert_eq!(text(&result.unwrap()), m.name, "{}", m.name),
            _ => assert!(result.is_err(), "{} must not decrypt twice", m.name),
        }
    }

    // What decryption changed, written back the way the client will, survives a reload.
    let carol = address("333333333333333@lid");
    store::save_session(&pool, &carol, &signal.sessions[&carol])
        .await
        .unwrap();
    store::save_identity(&pool, &carol, &signal.identities[&carol])
        .await
        .unwrap();
    store::remove_pre_key(&pool, 3).await.unwrap();
    let reloaded = store::load(&pool).await.unwrap().unwrap();
    assert!(reloaded.sessions.iter().any(|(a, _)| *a == carol));
    assert!(!reloaded.pre_keys.iter().any(|(id, _)| *id == 3));
    let c1 = pending.iter().find(|m| m.name == "c1").unwrap();
    let replay = reloaded
        .signal_store()
        .decrypt_prekey(&carol, &B64.decode(&c1.ciphertext).unwrap());
    assert!(
        matches!(replay, Err(SignalError::Duplicate { .. })),
        "{replay:?}"
    );
}

#[tokio::test]
async fn a_role_granted_like_the_live_mcp_cannot_read_the_keys() {
    let Some((pool, server, _turn)) = database().await else {
        return;
    };
    // The live grants: SELECT on all of `public`, and on every future table there.
    for sql in [
        "DROP ROLE IF EXISTS mcp_probe",
        "CREATE ROLE mcp_probe LOGIN PASSWORD 'probe'",
        "GRANT CONNECT ON DATABASE client_test TO mcp_probe",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO mcp_probe",
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    store::migrate(&pool).await.unwrap();
    store::save(&pool, &baileys::import(&fixtures().join("alice")).unwrap())
        .await
        .unwrap();

    let host = server.trim_start_matches("postgresql://postgres:postgres@");
    let probe = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("postgresql://mcp_probe:probe@{host}/{DATABASE}"))
        .await
        .unwrap();
    for table in ["account", "pre_key", "session", "identity", "sender_key"] {
        let query = match table {
            "account" => "SELECT count(*) FROM device.account",
            "pre_key" => "SELECT count(*) FROM device.pre_key",
            "session" => "SELECT count(*) FROM device.session",
            "identity" => "SELECT count(*) FROM device.identity",
            _ => "SELECT count(*) FROM device.sender_key",
        };
        let error = sqlx::query_scalar::<_, i64>(query)
            .fetch_one(&probe)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("permission denied for schema device"),
            "{table}: {error}"
        );
    }
    probe.close().await;
    for sql in ["ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE SELECT ON TABLES FROM mcp_probe"]
    {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
}

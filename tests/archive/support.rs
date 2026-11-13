//! Test fixtures — the Rust mirror of `tests/conftest.py`.
//!
//! Postgres tests run against a throwaway testcontainer and report a loud skip when
//! Docker is unreachable. They must never reach the homelab database: **nothing here
//! reads a DSN from the environment**, the container builds its own. That is the
//! structural half of "never point tests at a real database", and it is why
//! `#[sqlx::test]` is not used anywhere — that macro wants `DATABASE_URL` from the
//! environment and creates databases itself, which re-opens the door `conftest.py`
//! deliberately nailed shut.
//!
//! The seeders here write raw SQL rather than calling the store, because the store's
//! writers are the ingester's and are not ported in this phase. When phase 2 ports
//! them, these helpers are what should move onto them.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use sqlx::postgres::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, OnceCell};
use whatsapp_core::db::{create_pool, ensure_schema};

/// `conftest.py`'s TABLES, plus `message_revision`, which did not exist when that list
/// was written and which `message` would take with it through the CASCADE anyway.
/// `schema_migration` is deliberately absent: truncating it would make the shared schema
/// look unapplied to the next test.
const TRUNCATE_ALL: &str = "TRUNCATE message_revision, message, reaction, participant, chat, \
                            alias_member, user_alias, sync_state RESTART IDENTITY CASCADE";

/// One named container serves every run. It is parked in a `static`, which Rust never
/// drops, so it is marked for reuse rather than reaping: the next run attaches to the
/// same container instead of leaving a second one behind. Remove it with
/// `docker rm -f whatsapp-sync-test-pg` when you want the disk back.
const CONTAINER_NAME: &str = "whatsapp-sync-test-pg";

static CONTAINER: OnceCell<Option<(ContainerAsync<Postgres>, String)>> = OnceCell::const_new();
/// One database, many tests, and each one truncates: they have to take turns.
static TURN: Mutex<()> = Mutex::const_new(());

pub struct Fixture {
    pub pool: PgPool,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

/// A pool over an empty database, or `None` when Docker is unreachable.
///
/// Returning `None` rather than failing is deliberate: Postgres tests skip when Docker
/// is not there. Rust has no "skipped" outcome, so each caller prints the reason and
/// returns — loudly enough to notice in a CI log.
pub async fn fixture() -> Option<Fixture> {
    let turn = TURN.lock().await;
    let dsn = CONTAINER
        .get_or_init(|| async {
            let request = Postgres::default()
                .with_tag("18-alpine")
                .with_container_name(CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always);
            match request.start().await {
                Ok(container) => {
                    let host = container.get_host().await.ok()?;
                    let port = container.get_host_port_ipv4(5432).await.ok()?;
                    let dsn = format!("postgresql://postgres:postgres@{host}:{port}/postgres");
                    let pool = create_pool(&dsn).await.ok()?;
                    ensure_schema(&pool).await.ok()?;
                    pool.close().await;
                    Some((container, dsn))
                }
                Err(error) => {
                    eprintln!("SKIPPING the postgres tests: docker is not reachable: {error}");
                    None
                }
            }
        })
        .await
        .as_ref()
        .map(|(_, dsn)| dsn.clone())?;

    let pool = create_pool(&dsn).await.expect("the container is up");
    sqlx::query(TRUNCATE_ALL)
        .execute(&pool)
        .await
        .expect("truncate");
    Some(Fixture { pool, _turn: turn })
}

/// Skip the body with a printed reason when Docker is unreachable.
macro_rules! db_test {
    ($name:ident, |$fixture:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($fixture) = crate::support::fixture().await else {
                return;
            };
            $body
        }
    };
}
pub(crate) use db_test;

/// The DSN of the shared container, for the tests that need a second database.
pub async fn container_dsn() -> Option<String> {
    CONTAINER.get()?.as_ref().map(|(_, dsn)| dsn.clone())
}

// ------------------------------------------------------------------ the Python's seed

/// The fixed instant `tests/test_store_reads.py` builds its messages around, so an
/// assertion about ordering never depends on how fast the test ran.
pub fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()
}

/// `base()` plus a whole number of seconds, which is the only resolution WhatsApp has.
pub fn at(second: i64) -> DateTime<Utc> {
    base() + TimeDelta::seconds(second)
}

pub const GROUP: &str = "120363421509452248@g.us";
pub const DM: &str = "447700900123@c.us";
pub const HETTY: &str = "447700900123@c.us";
pub const HETTY_LID: &str = "269@lid";
pub const NAMELESS: &str = "48999888777@c.us";

pub struct Participant<'a> {
    pub participant_id: &'a str,
    pub lid: Option<&'a str>,
    pub phone: Option<&'a str>,
    pub contact_name: Option<&'a str>,
    pub push_name: Option<&'a str>,
}

impl Default for Participant<'_> {
    fn default() -> Self {
        Self {
            participant_id: HETTY,
            lid: Some(HETTY_LID),
            phone: Some("447700900123"),
            contact_name: Some("Henrietta Adaeze Okonkwo"),
            push_name: Some("hettyo"),
        }
    }
}

pub fn sandy() -> Participant<'static> {
    Participant::default()
}

pub fn nameless() -> Participant<'static> {
    Participant {
        participant_id: NAMELESS,
        lid: None,
        phone: Some("48999888777"),
        contact_name: None,
        push_name: None,
    }
}

pub async fn participant(pool: &PgPool, row: Participant<'_>) {
    sqlx::query(
        "INSERT INTO participant (participant_id, lid, phone, contact_name, push_name) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(row.participant_id)
    .bind(row.lid)
    .bind(row.phone)
    .bind(row.contact_name)
    .bind(row.push_name)
    .execute(pool)
    .await
    .expect("insert participant");
}

pub async fn chat(pool: &PgPool, chat_id: &str, kind: &str, name: Option<&str>) {
    sqlx::query(
        "INSERT INTO chat (chat_id, kind, name, raw) VALUES ($1, $2, $3, '{}'::jsonb) \
         ON CONFLICT (chat_id) DO UPDATE SET kind = EXCLUDED.kind, name = EXCLUDED.name",
    )
    .bind(chat_id)
    .bind(kind)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert chat");
}

pub async fn watermark(pool: &PgPool, chat_id: &str, ts: DateTime<Utc>) {
    sqlx::query("UPDATE chat SET watermark_ts = $2 WHERE chat_id = $1")
        .bind(chat_id)
        .bind(ts)
        .execute(pool)
        .await
        .expect("set watermark");
}

pub struct Msg<'a> {
    pub chat_id: &'a str,
    pub message_id: &'a str,
    pub second: i64,
    pub from_me: bool,
    pub author_id: Option<&'a str>,
    pub body: Option<&'a str>,
    pub kind: &'a str,
    pub reply_to_id: Option<&'a str>,
    pub ack: Option<i32>,
    pub media_mime: Option<&'a str>,
    pub media_filename: Option<&'a str>,
    pub media_seconds: Option<i32>,
    pub transcript: Option<&'a str>,
    pub transcript_state: &'a str,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Default for Msg<'_> {
    fn default() -> Self {
        Self {
            chat_id: GROUP,
            message_id: "m1",
            second: 0,
            from_me: false,
            author_id: Some(HETTY_LID),
            body: Some("hello"),
            kind: "text",
            reply_to_id: None,
            ack: Some(2),
            media_mime: None,
            media_filename: None,
            media_seconds: None,
            transcript: None,
            transcript_state: "n/a",
            deleted_at: None,
        }
    }
}

pub async fn message(pool: &PgPool, msg: Msg<'_>) {
    sqlx::query(
        "INSERT INTO message (chat_id, message_id, ts, from_me, author_id, body, kind, \
         reply_to_id, ack, has_media, media_mime, media_filename, media_seconds, transcript, \
         transcript_state, deleted_at, raw) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 jsonb_build_object('id', $2::text)) \
         ON CONFLICT (chat_id, message_id) DO UPDATE SET body = EXCLUDED.body",
    )
    .bind(msg.chat_id)
    .bind(msg.message_id)
    .bind(at(msg.second))
    .bind(msg.from_me)
    .bind(msg.author_id)
    .bind(msg.body)
    .bind(msg.kind)
    .bind(msg.reply_to_id)
    .bind(msg.ack)
    .bind(msg.media_mime.is_some())
    .bind(msg.media_mime)
    .bind(msg.media_filename)
    .bind(msg.media_seconds)
    .bind(msg.transcript)
    .bind(msg.transcript_state)
    .bind(msg.deleted_at)
    .execute(pool)
    .await
    .expect("insert message");
}

/// A message at a given second with a given body, which is most of what the read tests
/// need.
pub async fn said(pool: &PgPool, chat_id: &str, message_id: &str, second: i64, body: &str) {
    message(
        pool,
        Msg {
            chat_id,
            message_id,
            second,
            body: Some(body),
            ..Msg::default()
        },
    )
    .await;
}

pub async fn reaction(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    actor_id: &str,
    emoji: &str,
    ts: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "INSERT INTO reaction (chat_id, message_id, actor_id, emoji, ts) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(actor_id)
    .bind(emoji)
    .bind(ts)
    .execute(pool)
    .await
    .expect("insert reaction");
}

pub async fn revision(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    seq: i32,
    text: Option<&str>,
    first_seen_at: Option<DateTime<Utc>>,
    superseded_at: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "INSERT INTO message_revision (chat_id, message_id, seq, text, first_seen_at, \
         superseded_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(seq)
    .bind(text)
    .bind(first_seen_at)
    .bind(superseded_at)
    .execute(pool)
    .await
    .expect("insert message_revision");
}

pub async fn deleted(pool: &PgPool, chat_id: &str, message_id: &str, at: DateTime<Utc>) {
    sqlx::query(
        "UPDATE message SET deleted_at = $3, missing_since = $3 \
         WHERE chat_id = $1 AND message_id = $2",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(at)
    .execute(pool)
    .await
    .expect("mark deleted");
}

pub async fn sync_state(
    pool: &PgPool,
    loop_name: &str,
    last_run_at: DateTime<Utc>,
    last_error: Option<&str>,
    last_error_at: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "INSERT INTO sync_state (loop, last_run_at, last_error, last_error_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(loop_name)
    .bind(last_run_at)
    .bind(last_error)
    .bind(last_error_at)
    .execute(pool)
    .await
    .expect("insert sync_state");
}

/// `tests/test_store_reads.py::seed`, which most of the read tests start from: two
/// participants, a group and a DM, and three messages.
pub async fn seed(pool: &PgPool) {
    participant(pool, sandy()).await;
    participant(pool, nameless()).await;
    chat(pool, GROUP, "group", Some("Book club regulars")).await;
    chat(pool, DM, "dm", None).await;
    said(pool, GROUP, "g1", 1, "pierogi tonight").await;
    message(
        pool,
        Msg {
            chat_id: GROUP,
            message_id: "g2",
            second: 2,
            author_id: Some(NAMELESS),
            body: Some("no herbata left"),
            ..Msg::default()
        },
    )
    .await;
    said(pool, DM, "d1", 3, "call me").await;
}

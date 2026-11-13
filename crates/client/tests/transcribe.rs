//! Voice notes: decrypting what WAHA's own Baileys encrypted, the whisper.cpp contract, and
//! the worker end to end against a mock CDN, a mock whisper server and a throwaway Postgres.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::json;
use sqlx::postgres::{PgPool, PgPoolOptions};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, MutexGuard, OnceCell};
use whatsapp_client::media::{MediaError, MediaRef, decrypt_audio};
use whatsapp_client::transcribe::{TranscribeError, Whisper, Worker};

struct Fixture {
    reference: MediaRef,
    encrypted: Vec<u8>,
    plaintext: Vec<u8>,
}

fn fixture() -> Fixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media.json");
    let f: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let b = |k: &str| B64.decode(f[k].as_str().unwrap()).unwrap();
    Fixture {
        reference: MediaRef {
            direct_path: Some("/v/t62.7117-24/good".into()),
            url: None,
            media_key: b("mediaKey"),
            file_enc_sha256: Some(b("fileEncSha256")),
            file_sha256: Some(b("fileSha256")),
        },
        encrypted: b("encrypted"),
        plaintext: b("plaintext"),
    }
}

#[test]
fn decrypts_what_baileys_encrypted() {
    let f = fixture();
    assert_eq!(
        decrypt_audio(&f.encrypted, &f.reference).unwrap(),
        f.plaintext
    );
}

#[test]
fn refuses_a_tampered_or_mismatched_file() {
    let f = fixture();
    let mut flipped = f.encrypted.clone();
    flipped[100] ^= 1;
    assert_eq!(
        decrypt_audio(&flipped, &f.reference),
        Err(MediaError::EncryptedHash)
    );

    // Without the hash the message names, the MAC still catches it.
    let unhashed = MediaRef {
        file_enc_sha256: None,
        ..f.reference.clone()
    };
    assert_eq!(decrypt_audio(&flipped, &unhashed), Err(MediaError::Mac));
    let mut bad_mac = f.encrypted.clone();
    *bad_mac.last_mut().unwrap() ^= 1;
    assert_eq!(decrypt_audio(&bad_mac, &unhashed), Err(MediaError::Mac));
    assert_eq!(decrypt_audio(&[0; 5], &unhashed), Err(MediaError::Mac));

    let wrong_plain = MediaRef {
        file_sha256: Some(vec![0; 32]),
        ..f.reference.clone()
    };
    assert_eq!(
        decrypt_audio(&f.encrypted, &wrong_plain),
        Err(MediaError::PlainHash)
    );
    let short_key = MediaRef {
        media_key: vec![0; 31],
        ..f.reference.clone()
    };
    assert_eq!(
        decrypt_audio(&f.encrypted, &short_key),
        Err(MediaError::BadKey)
    );
}

#[test]
fn reads_the_reference_from_our_rows_and_from_wahas() {
    let f = fixture();
    let ours = json!({"source": "whatsapp-client", "media": f.reference.to_json()});
    assert_eq!(MediaRef::from_raw(&ours), Some(f.reference.clone()));

    // WAHA keeps the Baileys message, its bytes as Node Buffers, in either JSON form.
    let buffer = |b: &[u8]| json!({"type": "Buffer", "data": B64.encode(b)});
    let array = |b: &[u8]| json!({"type": "Buffer", "data": b});
    let waha = json!({"_data": {"message": {"audioMessage": {
        "url": "https://mmg.whatsapp.net/v/t62.7117-24/good?x=1",
        "directPath": "/v/t62.7117-24/good",
        "mediaKey": buffer(&f.reference.media_key),
        "fileEncSha256": array(f.reference.file_enc_sha256.as_ref().unwrap()),
        "fileSha256": f.reference.file_sha256.as_ref().map(|b| B64.encode(b)),
    }}}});
    let read = MediaRef::from_raw(&waha).unwrap();
    assert_eq!(read.media_key, f.reference.media_key);
    assert_eq!(read.file_enc_sha256, f.reference.file_enc_sha256);
    assert_eq!(read.file_sha256, f.reference.file_sha256);
    assert_eq!(
        read.location("https://cdn").unwrap(),
        "https://cdn/v/t62.7117-24/good"
    );

    let url_only = MediaRef {
        direct_path: None,
        ..read.clone()
    };
    assert_eq!(
        url_only.location("https://cdn").unwrap(),
        "https://mmg.whatsapp.net/v/t62.7117-24/good?x=1"
    );
    assert_eq!(
        MediaRef {
            url: None,
            ..url_only
        }
        .location("x"),
        Err(MediaError::NoLocation)
    );
    assert_eq!(
        MediaRef::from_raw(&json!({"source": "whatsapp-client"})),
        None
    );
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// A whisper server that answers every request the same way, counting them.
async fn whisper(status: StatusCode, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let router = Router::new().route(
        "/inference",
        post(move |_: Bytes| async move {
            seen.fetch_add(1, Ordering::SeqCst);
            (status, body)
        }),
    );
    (serve(router).await, calls)
}

#[derive(Debug)]
enum Expect {
    Text(&'static str),
    Refused,
    Unavailable,
}

#[tokio::test]
async fn whisper_answers_mean_what_the_python_client_took_them_to_mean() {
    let cases = [
        (
            StatusCode::OK,
            r#"{"text": "  hello there \n"}"#,
            Expect::Text("hello there"),
        ),
        (StatusCode::OK, r#"{"text": null}"#, Expect::Text("")),
        (
            StatusCode::OK,
            r#"{"text": "hi", "error": ""}"#,
            Expect::Text("hi"),
        ),
        // whisper.cpp's answer to audio it cannot decode: a 500, with an error.
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error": "failed to read audio"}"#,
            Expect::Refused,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "loading model",
            Expect::Unavailable,
        ),
        (StatusCode::BAD_REQUEST, "nope", Expect::Refused),
    ];
    for (status, body, expected) in cases {
        let (url, _) = whisper(status, body).await;
        let got = Whisper::new(&url).transcribe(b"ogg".to_vec()).await;
        let ok = match (&expected, &got) {
            (Expect::Text(want), Ok(text)) => want == text,
            (Expect::Refused, Err(TranscribeError::Refused(_))) => true,
            (Expect::Unavailable, Err(TranscribeError::Unavailable(_))) => true,
            _ => false,
        };
        assert!(ok, "{status} {body}: expected {expected:?}, got {got:?}");
    }
    let closed = Whisper::new("http://127.0.0.1:9").transcribe(vec![1]).await;
    assert!(
        matches!(closed, Err(TranscribeError::Unavailable(_))),
        "{closed:?}"
    );
}

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
                    eprintln!(
                        "SKIPPING the transcription worker tests: docker is not reachable: {e}"
                    );
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
    sqlx::query("DROP DATABASE IF EXISTS transcribe_test WITH (FORCE)")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE transcribe_test")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{server}/transcribe_test"))
        .await
        .unwrap();
    whatsapp_core::db::ensure_schema(&pool).await.unwrap();
    Some((pool, turn))
}

const CHAT: &str = "48500000000@s.whatsapp.net";

async fn voice_note(pool: &PgPool, id: &str, ts: i64, raw: serde_json::Value) {
    whatsapp_core::store::writes::ensure_chat(pool, CHAT, "dm")
        .await
        .unwrap();
    let row = whatsapp_core::models::MessageRow {
        chat_id: CHAT.into(),
        message_id: id.into(),
        ts: chrono::DateTime::from_timestamp(ts, 0).unwrap(),
        from_me: false,
        author_id: Some(CHAT.into()),
        body: None,
        kind: "audio".into(),
        reply_to_id: None,
        ack: Some(2),
        has_media: true,
        media_mime: Some("audio/ogg; codecs=opus".into()),
        media_filename: None,
        media_seconds: Some(3),
        transcript_state: "pending".into(),
        raw,
    };
    whatsapp_core::store::writes::upsert_message(pool, &row)
        .await
        .unwrap();
}

type State = (String, String, Option<String>);

fn state(id: &str, state: &str, text: Option<&str>) -> State {
    (id.into(), state.into(), text.map(str::to_owned))
}

async fn states(pool: &PgPool) -> Vec<State> {
    sqlx::query_as(
        "SELECT message_id, transcript_state, transcript FROM message ORDER BY message_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A CDN holding the fixture's file at its path, and nothing else.
async fn cdn(encrypted: Vec<u8>) -> String {
    let router = Router::new()
        .route("/v/t62.7117-24/good", get(move || async move { encrypted }))
        .fallback(|| async { StatusCode::NOT_FOUND });
    serve(router).await
}

#[tokio::test]
async fn the_worker_transcribes_fails_and_defers_like_the_python_one() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let f = fixture();
    let mut gone = f.reference.clone();
    gone.direct_path = Some("/v/t62.7117-24/expired".into());
    voice_note(
        &pool,
        "a-ours",
        1_700_000_004,
        json!({"media": f.reference.to_json()}),
    )
    .await;
    voice_note(
        &pool,
        "b-waha",
        1_700_000_003,
        json!({"_data": {"message": {"audioMessage": {
            "directPath": "/v/t62.7117-24/good",
            "mediaKey": {"type": "Buffer", "data": B64.encode(&f.reference.media_key)},
        }}}}),
    )
    .await;
    voice_note(
        &pool,
        "c-gone",
        1_700_000_002,
        json!({"media": gone.to_json()}),
    )
    .await;
    voice_note(
        &pool,
        "d-no-reference",
        1_700_000_001,
        json!({"source": "whatsapp-client"}),
    )
    .await;
    let media_base = cdn(f.encrypted.clone()).await;

    // Whisper down: nothing is lost, nothing is marked, and the batch stops at the first.
    let (down, calls) = whisper(StatusCode::SERVICE_UNAVAILABLE, "").await;
    assert_eq!(
        Worker::with_media_base(pool.clone(), &down, &media_base)
            .drain()
            .await
            .unwrap(),
        0
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(states(&pool).await.iter().all(|(_, s, _)| s == "pending"));

    // Whisper up: it must be handed the decrypted audio, not the file.
    let plaintext = f.plaintext.clone();
    let heard = Arc::new(AtomicUsize::new(0));
    let counted = heard.clone();
    let router = Router::new().route(
        "/inference",
        post(move |body: Bytes| async move {
            if !body
                .windows(plaintext.len())
                .any(|w| w == plaintext.as_slice())
            {
                return (StatusCode::BAD_REQUEST, "not the decrypted audio");
            }
            // The form fields the Python client sent: JSON back, and no sampling.
            let has = |field: &[u8]| body.windows(field.len()).any(|w| w == field);
            let field = |name: &str, value: &str| format!("name=\"{name}\"\r\n\r\n{value}\r\n");
            if !has(field("response_format", "json").as_bytes())
                || !has(field("temperature", "0.0").as_bytes())
            {
                return (StatusCode::BAD_REQUEST, "not the form whisper expects");
            }
            counted.fetch_add(1, Ordering::SeqCst);
            (StatusCode::OK, r#"{"text": " hello "}"#)
        }),
    );
    let up = serve(router).await;
    assert_eq!(
        Worker::with_media_base(pool.clone(), &up, &media_base)
            .drain()
            .await
            .unwrap(),
        2
    );
    assert_eq!(heard.load(Ordering::SeqCst), 2);
    assert_eq!(
        states(&pool).await,
        [
            state("a-ours", "done", Some("hello")),
            state("b-waha", "done", Some("hello")),
            state("c-gone", "failed", None),
            state("d-no-reference", "failed", None),
        ]
    );
    // Nothing left: a second pass does no work.
    assert_eq!(
        Worker::with_media_base(pool.clone(), &up, &media_base)
            .drain()
            .await
            .unwrap(),
        0
    );
    assert_eq!(heard.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_cdn_out_of_reach_defers_and_silence_is_stored_as_no_text() {
    let Some((pool, _turn)) = database().await else {
        return;
    };
    let f = fixture();
    voice_note(
        &pool,
        "a",
        1_700_000_001,
        json!({"media": f.reference.to_json()}),
    )
    .await;
    let (silent, calls) = whisper(StatusCode::OK, r#"{"text": "  "}"#).await;

    let broken = serve(Router::new().fallback(|| async { StatusCode::BAD_GATEWAY })).await;
    assert_eq!(
        Worker::with_media_base(pool.clone(), &silent, &broken)
            .drain()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        Worker::with_media_base(pool.clone(), &silent, "http://127.0.0.1:9")
            .drain()
            .await
            .unwrap(),
        0
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(states(&pool).await, [state("a", "pending", None)]);

    let media_base = cdn(f.encrypted).await;
    assert_eq!(
        Worker::with_media_base(pool.clone(), &silent, &media_base)
            .drain()
            .await
            .unwrap(),
        1
    );
    assert_eq!(states(&pool).await, [state("a", "done", None)]);
}

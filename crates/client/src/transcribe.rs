//! Voice notes to text, the Python `whatsapp_sync/ingest/transcribe.py` moved here.
//!
//! The worker takes pending voice notes newest first, fetches each from WhatsApp's CDN by the
//! reference its row keeps, decrypts it, and asks the whisper.cpp server for the text. Only
//! the text is stored, and it feeds the same search column as message bodies.
//!
//! How each step fails decides what happens to the voice note, as it did in Python:
//!
//! - the whisper server refused the audio (an `error` in its answer, even on a 500), or the
//!   file is gone or does not decrypt: `failed`, never retried;
//! - the whisper server or the CDN could not be reached: the note stays `pending`, the rest of
//!   the batch is left for the next pass, which would only fail the same way.

use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::PgPool;
use whatsapp_core::store::writes;

use crate::media::{self, MediaRef};

/// A long voice note on CPU takes a while, and a slow answer is still a good answer.
const WHISPER_TIMEOUT: Duration = Duration::from_secs(300);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// The Python worker's batch size and cadence.
const DEFAULT_LIMIT: i64 = 20;
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TranscribeError {
    /// Nothing is known about the audio; try again later.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The audio was read and cannot be transcribed; retrying will not change that.
    #[error("refused: {0}")]
    Refused(String),
}

/// The whisper.cpp server's `/inference`, with the Python client's contract.
pub struct Whisper {
    http: reqwest::Client,
    url: String,
}

impl Whisper {
    pub fn new(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(WHISPER_TIMEOUT)
            .build()
            .expect("HTTP client");
        Self {
            http,
            url: format!("{}/inference", base.trim_end_matches('/')),
        }
    }

    pub async fn transcribe(&self, audio: Vec<u8>) -> Result<String, TranscribeError> {
        let file = reqwest::multipart::Part::bytes(audio)
            .file_name("voice.ogg")
            .mime_str("audio/ogg")
            .expect("a valid mime type");
        let form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("response_format", "json")
            .text("temperature", "0.0");
        let response = self
            .http
            .post(&self.url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| TranscribeError::Unavailable(e.to_string()))?;
        let status = response.status();
        // whisper.cpp answers audio it cannot decode with a 500 and an `error` field, the same
        // status class as a server falling over, so the body decides which it was.
        let body: Value = response.json().await.unwrap_or(Value::Null);
        // Python's truthiness: an empty or false `error` is no error.
        if let Some(error) = body
            .get("error")
            .filter(|e| !matches!(e, Value::Null | Value::Bool(false)) && e.as_str() != Some(""))
        {
            return Err(TranscribeError::Refused(format!(
                "whisper refused the audio: {error}"
            )));
        }
        if status.is_server_error() {
            return Err(TranscribeError::Unavailable(format!("HTTP {status}")));
        }
        if !status.is_success() {
            return Err(TranscribeError::Refused(format!("HTTP {status}")));
        }
        Ok(body
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned())
    }
}

pub struct Worker {
    pool: PgPool,
    whisper: Whisper,
    cdn: reqwest::Client,
    media_base: String,
}

impl Worker {
    pub fn new(pool: PgPool, whisper_url: &str) -> Self {
        Self::with_media_base(pool, whisper_url, media::MEDIA_BASE)
    }

    /// For tests: a CDN at another address.
    pub fn with_media_base(pool: PgPool, whisper_url: &str, media_base: &str) -> Self {
        let cdn = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .expect("HTTP client");
        Self {
            pool,
            whisper: Whisper::new(whisper_url),
            cdn,
            media_base: media_base.to_owned(),
        }
    }

    pub async fn run(self, interval: Duration) {
        loop {
            let error = match self.drain().await {
                Ok(done) => {
                    if done != 0 {
                        tracing::info!("transcribed {done} voice note(s)");
                    }
                    None
                }
                Err(e) => {
                    tracing::error!("transcription pass failed: {e}");
                    Some(e.to_string())
                }
            };
            if let Err(e) = writes::set_sync_state(&self.pool, "transcribe", error.as_deref()).await
            {
                tracing::warn!("recording the transcription pass failed: {e}");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// One pass over the queue. Returns how many were transcribed.
    pub async fn drain(&self) -> Result<usize, sqlx::Error> {
        let mut done = 0;
        for (chat_id, message_id, raw) in
            writes::pending_transcripts(&self.pool, DEFAULT_LIMIT).await?
        {
            match self.one(&raw).await {
                Ok(text) => {
                    let text = Some(text.as_str()).filter(|t| !t.is_empty());
                    writes::set_transcript(&self.pool, &chat_id, &message_id, text, "done").await?;
                    done += 1;
                }
                Err(TranscribeError::Unavailable(why)) => {
                    tracing::warn!("transcription unavailable, deferring the queue: {why}");
                    break;
                }
                Err(TranscribeError::Refused(why)) => {
                    tracing::warn!("transcription failed for one voice note: {why}");
                    writes::set_transcript(&self.pool, &chat_id, &message_id, None, "failed")
                        .await?;
                }
            }
        }
        Ok(done)
    }

    async fn one(&self, raw: &Value) -> Result<String, TranscribeError> {
        let reference = MediaRef::from_raw(raw)
            .ok_or_else(|| TranscribeError::Refused("no media reference".into()))?;
        let location = reference
            .location(&self.media_base)
            .map_err(|e| TranscribeError::Refused(e.to_string()))?;
        let response = self
            .cdn
            .get(&location)
            .send()
            .await
            .map_err(|e| TranscribeError::Unavailable(e.to_string()))?;
        let status = response.status();
        if matches!(status.as_u16(), 403 | 404 | 410) {
            return Err(TranscribeError::Refused(format!(
                "the CDN no longer has it (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Err(TranscribeError::Unavailable(format!("CDN HTTP {status}")));
        }
        let file = response
            .bytes()
            .await
            .map_err(|e| TranscribeError::Unavailable(e.to_string()))?;
        let audio = media::decrypt_audio(&file, &reference)
            .map_err(|e| TranscribeError::Refused(e.to_string()))?;
        self.whisper.transcribe(audio).await
    }
}

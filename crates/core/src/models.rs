//! The row types the read surface hands to the renderer.
//!
//! One struct per query shape, mirroring the columns `store` selects. The write side starts
//! with [`MessageRow`], the Rust client's row for `message`, column for column the Python
//! ingester's (`whatsapp_sync/rows.py`).
//!
//! `raw` is not selected anywhere here: the renderer describes media, it never fetches
//! it, so the stored payload has no reader on this side.

use chrono::{DateTime, Utc};

/// One chat as `chats` shows it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatSummary {
    pub chat_id: String,
    pub kind: String,
    /// `COALESCE` ends at `c.chat_id`, so this is never NULL.
    pub display_name: String,
    pub last_ts: Option<DateTime<Utc>>,
    pub message_count: i64,
}

/// One message as `messages`, `message` and the history renderer see it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HistoryRow {
    pub chat_id: String,
    pub message_id: String,
    pub ts: DateTime<Utc>,
    pub from_me: bool,
    pub kind: String,
    pub body: Option<String>,
    pub transcript: Option<String>,
    pub media_mime: Option<String>,
    pub media_filename: Option<String>,
    pub media_seconds: Option<i32>,
    pub reply_to_id: Option<String>,
    pub ack: Option<i32>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// Derived, never stored: "was this edited" is exactly "is there a history of it".
    pub edited: bool,
    /// NULL when the message carries no sender at all, which a group message can.
    pub author: Option<String>,
    /// `emoji Name, emoji Name` — who reacted, not just what was left behind.
    pub reactions: Option<String>,
}

/// One search hit: the matched message plus the snippet Postgres highlighted.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchHit {
    pub chat_id: String,
    pub message_id: String,
    pub ts: DateTime<Utc>,
    pub from_me: bool,
    pub chat_name: String,
    pub author: Option<String>,
    /// `ts_headline` over `coalesce(body,'') || ' ' || coalesce(transcript,'')`, so the
    /// input is never NULL and neither is this.
    pub snippet: String,
}

/// One person as `people` shows them, with every name form they are known under.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Person {
    pub participant_id: String,
    pub lid: Option<String>,
    pub phone: Option<String>,
    pub contact_name: Option<String>,
    pub push_name: Option<String>,
    pub display_name: String,
    pub aliases: Option<String>,
    /// The id `messages` takes. Participants are keyed `@c.us` and DM chats
    /// `@s.whatsapp.net`, so a `participant_id` is never a usable `chat_id`.
    pub chat_id: Option<String>,
}

/// One ingester loop's state, with the corpus-wide counts beside it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StatusRow {
    /// `loop` is a Rust keyword; the column is not.
    #[sqlx(rename = "loop")]
    pub loop_name: String,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub chats: i64,
    pub messages: i64,
    pub oldest_watermark: Option<DateTime<Utc>>,
    pub pending_transcripts: i64,
    pub failed_transcripts: i64,
    pub deleted: i64,
    pub edited: i64,
}

/// One revision of one message: what it said, and for how long.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Revision {
    pub seq: i32,
    /// Nullable because a message can be edited from having text to having none.
    pub text: Option<String>,
    /// NULL on seq 1 and only there, and it means "unknown", not "missing".
    pub first_seen_at: Option<DateTime<Utc>>,
    /// NULL marks the revision that is current.
    pub superseded_at: Option<DateTime<Utc>>,
}

/// One `message` row as the ingester writes it. Every field is a column; `raw` is the client's
/// own record of the message, which nothing reads back.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MessageRow {
    pub chat_id: String,
    pub message_id: String,
    pub ts: DateTime<Utc>,
    pub from_me: bool,
    pub author_id: Option<String>,
    pub body: Option<String>,
    pub kind: String,
    pub reply_to_id: Option<String>,
    pub ack: Option<i32>,
    pub has_media: bool,
    pub media_mime: Option<String>,
    pub media_filename: Option<String>,
    pub media_seconds: Option<i32>,
    pub transcript_state: String,
    pub raw: serde_json::Value,
}

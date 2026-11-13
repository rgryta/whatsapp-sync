//! The ingester's writes, for the Rust client that replaces WAHA and the Python ingester.
//!
//! Each is the Python `whatsapp_sync/store.py` statement it replaces, with the same rules:
//! `ack` only climbs, `body` changes only through [`record_revision`] so every edit leaves
//! history, and a transcript is never reset by a re-delivery. What is new is what the client
//! learns that WAHA's polling never could: an explicit deletion ([`mark_revoked`]) and single
//! reactions as they happen ([`set_reaction`]), where the Python side inferred both from
//! snapshots.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;

use crate::models::MessageRow;

/// A chat the archive has not seen yet. Its name stays empty until the address book or a
/// group subject supplies one; the chat must exist first because `message` references it.
pub async fn ensure_chat(pool: &PgPool, chat_id: &str, kind: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO chat (chat_id, kind) VALUES ($1, $2) ON CONFLICT (chat_id) DO NOTHING",
    )
    .bind(chat_id)
    .bind(kind)
    .execute(pool)
    .await?;
    Ok(())
}

/// `upsert_messages`, one row. On conflict neither `body` nor the transcript is touched, and
/// `ack` only climbs.
pub async fn upsert_message(pool: &PgPool, row: &MessageRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO message (
           chat_id, message_id, ts, from_me, author_id, body, kind, reply_to_id, ack,
           has_media, media_mime, media_filename, media_seconds, transcript_state, raw
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
         ON CONFLICT (chat_id, message_id) DO UPDATE SET
           ts             = EXCLUDED.ts,
           from_me        = EXCLUDED.from_me,
           author_id      = EXCLUDED.author_id,
           kind           = EXCLUDED.kind,
           reply_to_id    = EXCLUDED.reply_to_id,
           ack            = GREATEST(COALESCE(message.ack, -1), EXCLUDED.ack),
           has_media      = EXCLUDED.has_media,
           media_mime     = EXCLUDED.media_mime,
           media_filename = EXCLUDED.media_filename,
           media_seconds  = EXCLUDED.media_seconds,
           raw            = EXCLUDED.raw",
    )
    .bind(&row.chat_id)
    .bind(&row.message_id)
    .bind(row.ts)
    .bind(row.from_me)
    .bind(&row.author_id)
    .bind(&row.body)
    .bind(&row.kind)
    .bind(&row.reply_to_id)
    .bind(row.ack)
    .bind(row.has_media)
    .bind(&row.media_mime)
    .bind(&row.media_filename)
    .bind(row.media_seconds)
    .bind(&row.transcript_state)
    .bind(&row.raw)
    .execute(pool)
    .await?;
    Ok(())
}

/// `set_watermark`: never backwards. `status` reports the oldest one.
pub async fn set_watermark(
    pool: &PgPool,
    chat_id: &str,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE chat SET watermark_ts = GREATEST($2, COALESCE(watermark_ts, $2)), \
         last_synced_at = now() WHERE chat_id = $1",
    )
    .bind(chat_id)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(())
}

/// `record_revision`: an edit, as history. One transaction over the locked message row: `seq`
/// is max+1, and the log must never disagree with the body it describes. The first edit also
/// records the text it replaces, as seq 1 with an unknown start. NULL and '' are the same
/// absence of text, and neither is an edit. Returns whether anything was written.
pub async fn record_revision(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    text: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT body FROM message WHERE chat_id = $1 AND message_id = $2 FOR UPDATE",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((body,)) = current else {
        return Ok(false);
    };
    if body.as_deref().unwrap_or("") == text.unwrap_or("") {
        return Ok(false);
    }
    let mut seq: i32 = sqlx::query_scalar(
        "SELECT coalesce(max(seq), 0) FROM message_revision WHERE chat_id = $1 AND message_id = $2",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await?;
    if seq == 0 {
        sqlx::query(
            "INSERT INTO message_revision (chat_id, message_id, seq, text, first_seen_at, superseded_at) \
             VALUES ($1, $2, 1, $3::text, NULL, now())",
        )
        .bind(chat_id)
        .bind(message_id)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
        seq = 1;
    } else {
        sqlx::query(
            "UPDATE message_revision SET superseded_at = now() \
             WHERE chat_id = $1 AND message_id = $2 AND superseded_at IS NULL",
        )
        .bind(chat_id)
        .bind(message_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO message_revision (chat_id, message_id, seq, text, first_seen_at, superseded_at) \
         VALUES ($1, $2, $3, $4::text, now(), NULL)",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(seq + 1)
    .bind(text)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE message SET body = $3::text WHERE chat_id = $1 AND message_id = $2")
        .bind(chat_id)
        .bind(message_id)
        .bind(text)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// `refresh_receipts`, one message: delivered, read and played only ever climb.
pub async fn raise_ack(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    ack: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE message SET ack = GREATEST(COALESCE(ack, -1), $3::int) WHERE chat_id = $1 AND message_id = $2")
        .bind(chat_id)
        .bind(message_id)
        .bind(ack)
        .execute(pool)
        .await?;
    Ok(())
}

/// A message deleted for everyone. The sender's own revoke is evidence, so unlike the Python
/// sweep's two-strike absence it is marked deleted at once. Returns whether it was archived.
/// The archived id of the message an edit or deletion points at: `expected` when a row has
/// it, else the row in the chat with WhatsApp's own `stanza_id` (the third `_` field of every
/// id; JIDs hold no `_`). An edit from the phone in this account's own chat can name the chat
/// in another JID form than the message was archived under, so the rebuilt id misses.
pub async fn resolve_message_id(
    pool: &PgPool,
    chat_id: &str,
    expected: &str,
    stanza_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT message_id FROM message \
         WHERE chat_id = $1 AND (message_id = $2 OR split_part(message_id, '_', 3) = $3) \
         ORDER BY message_id = $2 DESC LIMIT 1",
    )
    .bind(chat_id)
    .bind(expected)
    .bind(stanza_id)
    .fetch_optional(pool)
    .await
}

/// One archived message's `raw`.
pub async fn message_raw(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar("SELECT raw FROM message WHERE chat_id = $1 AND message_id = $2")
        .bind(chat_id)
        .bind(message_id)
        .fetch_optional(pool)
        .await
}

pub async fn mark_revoked(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE message SET deleted_at = COALESCE(deleted_at, now()), missing_since = NULL \
         WHERE chat_id = $1 AND message_id = $2",
    )
    .bind(chat_id)
    .bind(message_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// One person's reaction to one message, replacing their previous one. Skipped when the
/// message is not archived: the reaction table references it. Returns whether it was stored.
pub async fn set_reaction(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    actor_id: &str,
    emoji: &str,
    ts: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO reaction (chat_id, message_id, actor_id, emoji, ts) \
         SELECT $1, $2, $3, $4, $5 WHERE EXISTS \
           (SELECT 1 FROM message WHERE chat_id = $1 AND message_id = $2) \
         ON CONFLICT (chat_id, message_id, actor_id) DO UPDATE SET emoji = EXCLUDED.emoji, ts = EXCLUDED.ts",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(actor_id)
    .bind(emoji)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A reaction taken back: WhatsApp sends it as an empty emoji.
pub async fn remove_reaction(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    actor_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM reaction WHERE chat_id = $1 AND message_id = $2 AND actor_id = $3")
        .bind(chat_id)
        .bind(message_id)
        .bind(actor_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `upsert_observed_participants`, one author: matched by id or LID, because the same person
/// may already exist under their phone JID with this LID, and a blind insert would violate
/// `lid UNIQUE`. The push name only fills in, never erases.
pub async fn upsert_observed_participant(
    pool: &PgPool,
    participant_id: &str,
    lid: Option<&str>,
    push_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    let existing: Option<String> = sqlx::query_scalar(
        "UPDATE participant SET push_name = COALESCE($2, push_name), last_seen = now() \
         WHERE participant_id = $1 OR lid = $1 RETURNING participant_id",
    )
    .bind(participant_id)
    .bind(push_name)
    .fetch_optional(pool)
    .await?;
    if existing.is_none() {
        sqlx::query(
            "INSERT INTO participant (participant_id, lid, push_name) VALUES ($1, $2, $3) \
             ON CONFLICT (participant_id) DO NOTHING",
        )
        .bind(participant_id)
        .bind(lid)
        .bind(push_name)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// `set_sync_state`: success records the time but keeps the last error visible.
pub async fn set_sync_state(
    pool: &PgPool,
    loop_name: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sync_state (loop, last_run_at, last_error, last_error_at) \
         VALUES ($1, now(), $2, CASE WHEN $2::text IS NULL THEN NULL ELSE now() END) \
         ON CONFLICT (loop) DO UPDATE SET \
           last_run_at   = now(), \
           last_error    = COALESCE($2, sync_state.last_error), \
           last_error_at = CASE WHEN $2::text IS NULL THEN sync_state.last_error_at ELSE now() END",
    )
    .bind(loop_name)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// `list_pending_transcripts`, with each voice note's `raw`: the media reference the
/// transcriber fetches the audio by. Newest first, as the Python worker took them.
pub async fn pending_transcripts(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(String, String, serde_json::Value)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT chat_id, message_id, raw FROM message WHERE transcript_state = 'pending' \
         ORDER BY ts DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// `set_transcript`.
pub async fn set_transcript(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
    text: Option<&str>,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE message SET transcript = $3, transcript_state = $4 WHERE chat_id = $1 AND message_id = $2")
        .bind(chat_id)
        .bind(message_id)
        .bind(text)
        .bind(state)
        .execute(pool)
        .await?;
    Ok(())
}

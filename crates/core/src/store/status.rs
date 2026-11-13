//! `sync_state`, with the corpus-wide counts `status` reports beside it.

use sqlx::postgres::PgPool;

use crate::models::StatusRow;

pub async fn sync_status(pool: &PgPool) -> Result<Vec<StatusRow>, sqlx::Error> {
    sqlx::query_as::<_, StatusRow>(
        "
        SELECT s.loop, s.last_run_at, s.last_error, s.last_error_at,
               (SELECT count(*) FROM chat) AS chats,
               (SELECT count(*) FROM message) AS messages,
               (SELECT min(watermark_ts) FROM chat WHERE watermark_ts IS NOT NULL)
                 AS oldest_watermark,
               (SELECT count(*) FROM message WHERE transcript_state = 'pending')
                 AS pending_transcripts,
               -- Reported beside the pending count because \"0 pending\" reads as \"every
               -- voice note is transcribed\", and on the live archive 65 of 78 are not.
               -- `failed` is terminal by design: WAHA answers those with
               -- `Failed to re-upload media (2)` and no url, because WhatsApp no longer
               -- holds the audio. They predate the transcriber, and a retry cannot
               -- bring the bytes back - so the number has to be visible instead.
               (SELECT count(*) FROM message WHERE transcript_state = 'failed')
                 AS failed_transcripts,
               (SELECT count(*) FROM message WHERE deleted_at IS NOT NULL) AS deleted,
               (SELECT count(DISTINCT (chat_id, message_id)) FROM message_revision)
                 AS edited
        FROM sync_state s ORDER BY s.loop
        ",
    )
    .fetch_all(pool)
    .await
}

//! `chat`: the listing, with each chat's activity counted from the `message` table.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;

use crate::models::ChatSummary;

/// Chats by last activity.
///
/// `since` filters the messages joined, not the chats listed, so the count becomes "how
/// many arrived since then" and chats with nothing new drop out entirely. That is the
/// catch-up question, answered by the tool that already lists chats.
pub async fn list_chats(
    pool: &PgPool,
    limit: i64,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<ChatSummary>, sqlx::Error> {
    sqlx::query_as::<_, ChatSummary>(concat!(
        "
        SELECT c.chat_id, c.kind, ",
        chat_name!(),
        " AS display_name,
               max(m.ts) AS last_ts, count(m.*) AS message_count
        FROM chat c
        LEFT JOIN message m
               ON m.chat_id = c.chat_id
              AND ($2::timestamptz IS NULL OR m.ts >= $2)
        GROUP BY c.chat_id, c.kind, c.name
        -- Without `since`, an empty chat is still a chat and stays listed. With it,
        -- a chat holding nothing new is exactly what the caller is filtering out.
        HAVING $2::timestamptz IS NULL OR count(m.*) > 0
        ORDER BY last_ts DESC NULLS LAST
        LIMIT $1
        "
    ))
    .bind(limit)
    .bind(since)
    .fetch_all(pool)
    .await
}

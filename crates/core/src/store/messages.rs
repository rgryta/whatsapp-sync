//! `message` and `message_revision`: one page of a chat, one message, the search, and
//! one message's history.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;

use crate::models::{HistoryRow, Revision, SearchHit};

// One page of a chat, handed back oldest first, which is reading order. Two statements,
// not one with a computed direction, because no caller value may reach the SQL text.
//
// With `after` the page is the *oldest* `limit` messages after it: a caller who names a
// start wants to read forward from there, and handing back the end of the period
// instead reads as though everything before it were missing. Without `after` it is the
// newest `limit`, so opening a chat shows what is going on now rather than the day it
// began.
//
// Ordered by `(ts, message_id)` throughout, because WhatsApp timestamps are whole
// seconds and `ts` alone ties.
const HISTORY_FROM_OLDEST: &str = concat!(
    "
    SELECT * FROM (
      SELECT ",
    message_columns!(),
    "
      FROM message m
      WHERE m.chat_id = $1
        AND ($2::timestamptz IS NULL OR m.ts < $2)
        AND ($3::timestamptz IS NULL OR m.ts > $3)
      ORDER BY m.ts, m.message_id
      LIMIT $4
    ) page ORDER BY ts, message_id
"
);

const HISTORY_NEWEST: &str = concat!(
    "
    SELECT * FROM (
      SELECT ",
    message_columns!(),
    "
      FROM message m
      WHERE m.chat_id = $1
        AND ($2::timestamptz IS NULL OR m.ts < $2)
        AND ($3::timestamptz IS NULL OR m.ts > $3)
      ORDER BY m.ts DESC, m.message_id DESC
      LIMIT $4
    ) page ORDER BY ts, message_id
"
);

/// A page of messages, oldest first. See [`HISTORY_FROM_OLDEST`] for which page.
///
/// [`count_history`] says how many messages the same window holds in all.
pub async fn get_history(
    pool: &PgPool,
    chat_id: &str,
    limit: i64,
    before: Option<DateTime<Utc>>,
    after: Option<DateTime<Utc>>,
) -> Result<Vec<HistoryRow>, sqlx::Error> {
    let sql = if after.is_some() {
        HISTORY_FROM_OLDEST
    } else {
        HISTORY_NEWEST
    };
    sqlx::query_as::<_, HistoryRow>(sql)
        .bind(chat_id)
        .bind(before)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Every message in the window [`get_history`] pages through.
pub async fn count_history(
    pool: &PgPool,
    chat_id: &str,
    before: Option<DateTime<Utc>>,
    after: Option<DateTime<Utc>>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "
        SELECT count(*) FROM message m
        WHERE m.chat_id = $1
          AND ($2::timestamptz IS NULL OR m.ts < $2)
          AND ($3::timestamptz IS NULL OR m.ts > $3)
        ",
    )
    .bind(chat_id)
    .bind(before)
    .bind(after)
    .fetch_one(pool)
    .await
}

/// One message by id, as a list: absent is an empty page, which the renderer already
/// says "(nothing)" to.
pub async fn get_message(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
) -> Result<Vec<HistoryRow>, sqlx::Error> {
    sqlx::query_as::<_, HistoryRow>(concat!(
        "
        SELECT ",
        message_columns!(),
        "
        FROM message m WHERE m.chat_id = $1 AND m.message_id = $2
        "
    ))
    .bind(chat_id)
    .bind(message_id)
    .fetch_all(pool)
    .await
}

/// Full text over bodies and transcripts, optionally narrowed to one sender.
///
/// `before`/`after` bound the period. Ranking stays by relevance, so a date window is a
/// filter on what may match, not a reordering: without it the best matches of all time
/// crowd out a period the reader actually asked about.
///
/// Deleted messages are excluded rather than shown withheld. A search result is a
/// snippet of the matching text, so returning one would print exactly what the default
/// views are supposed to hold back, and returning a blank hit would be useless. The
/// text is still reachable through the revision history, which is where asking for it
/// is explicit.
pub async fn search_messages(
    pool: &PgPool,
    query: &str,
    person: Option<&str>,
    chat_id: Option<&str>,
    before: Option<DateTime<Utc>>,
    after: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<SearchHit>, sqlx::Error> {
    sqlx::query_as::<_, SearchHit>(concat!(
        "
        SELECT m.chat_id, m.message_id, m.ts, m.from_me,
               ",
        chat_name!(),
        " AS chat_name,
               ",
        author!(),
        " AS author,
               ts_headline('simple',
                 coalesce(m.body,'') || ' ' || coalesce(m.transcript,''),
                 websearch_to_tsquery('simple', $1)) AS snippet
        FROM message m JOIN chat c ON c.chat_id = m.chat_id
        WHERE m.search @@ websearch_to_tsquery('simple', $1)
          AND m.deleted_at IS NULL
          AND ($2::text IS NULL OR m.chat_id = $2)
          AND ($3::text IS NULL OR EXISTS (
                SELECT 1 FROM participant p
                 WHERE (p.participant_id = m.author_id OR p.lid = m.author_id)
                   AND ",
        person_match!("$3"),
        "))
          AND ($4::timestamptz IS NULL OR m.ts < $4)
          AND ($5::timestamptz IS NULL OR m.ts > $5)
        ORDER BY ts_rank_cd(m.search, websearch_to_tsquery('simple', $1)) DESC,
                 m.ts DESC, m.message_id
        LIMIT $6
        "
    ))
    .bind(query)
    .bind(chat_id)
    .bind(person)
    .bind(before)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One message's text over time, oldest first.
pub async fn list_revisions(
    pool: &PgPool,
    chat_id: &str,
    message_id: &str,
) -> Result<Vec<Revision>, sqlx::Error> {
    sqlx::query_as::<_, Revision>(
        "
        SELECT seq, text, first_seen_at, superseded_at
          FROM message_revision WHERE chat_id = $1 AND message_id = $2 ORDER BY seq
        ",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_all(pool)
    .await
}

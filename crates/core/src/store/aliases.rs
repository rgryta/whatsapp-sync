//! `user_alias` and `alias_member` — the MCP role's only write authority.
//!
//! `0001_init.sql` grants `whatsapp_mcp` SELECT everywhere and INSERT/UPDATE/DELETE on
//! exactly these two tables, so this file is the whole of what the read surface can
//! change.

use sqlx::postgres::PgPool;

/// Create or replace an alias and its member set.
///
/// Members are replaced wholesale: the argument is the intended set, not an add. The
/// note is replaced too, including with nothing — re-setting an alias without a note
/// clears the note it had. That is the Python behaviour, kept rather than quietly
/// improved, because the stored note is not visible through any tool this server
/// serves and changing it here would be a change nobody could see.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    participant_ids: &[String],
    note: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO user_alias (alias, note) VALUES ($1, $2) \
         ON CONFLICT (alias) DO UPDATE SET note = EXCLUDED.note, updated_at = now()",
    )
    .bind(alias)
    .bind(note)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM alias_member WHERE alias = $1")
        .bind(alias)
        .execute(&mut *tx)
        .await?;
    // One statement for the whole set rather than a loop: `unnest` is what asyncpg's
    // `executemany` amounts to here, with one round trip instead of one per member.
    sqlx::query(
        "INSERT INTO alias_member (alias, participant_id) \
         SELECT $1, member FROM unnest($2::text[]) AS member \
         ON CONFLICT DO NOTHING",
    )
    .bind(alias)
    .bind(participant_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Remove an alias, reporting whether it existed. `alias_member` goes with it through
/// the CASCADE.
pub async fn delete_alias(pool: &PgPool, alias: &str) -> Result<bool, sqlx::Error> {
    let removed: Option<String> =
        sqlx::query_scalar("DELETE FROM user_alias WHERE alias = $1 RETURNING alias")
            .bind(alias)
            .fetch_optional(pool)
            .await?;
    Ok(removed.is_some())
}

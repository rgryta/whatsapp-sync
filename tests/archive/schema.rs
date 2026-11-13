//! The schema applier and the constraints the archive leans on.
//!
//! Ported from `tests/test_db.py`. The migrations are embedded with `include_str!`, so
//! "a migration is missing" is tested against a fresh database in `server.rs` rather
//! than against a temporary directory of SQL files.

use whatsapp_core::db::{applied_migrations, assert_migrations, ensure_schema};

use crate::support::db_test;

db_test!(ensure_schema_is_idempotent, |f| {
    // The fixture already applied them, so a second pass must apply nothing.
    let applied_again = ensure_schema(&f.pool).await.expect("ensure_schema");
    assert!(applied_again.is_empty(), "{applied_again:?}");
    let names = applied_migrations(&f.pool).await.expect("applied");
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(names, unique, "one row per file, in filename order");
    assert!(names.contains(&"0001_init.sql".to_owned()), "{names:?}");
});

db_test!(assert_migrations_passes_on_a_current_schema, |f| {
    assert_migrations(&f.pool).await.expect("current");
});

db_test!(
    the_tablespace_migration_is_a_noop_without_the_tablespaces,
    |f| {
        // The test container has neither tablespace; the migration must not fail there.
        let applied = applied_migrations(&f.pool).await.expect("applied");
        assert!(
            applied.contains(&"0002_place_on_tablespaces.sql".to_owned()),
            "{applied:?}"
        );
        let misplaced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_class \
             WHERE relname IN ('message','chat') AND reltablespace <> 0",
        )
        .fetch_one(&f.pool)
        .await
        .expect("pg_class");
        assert_eq!(misplaced, 0);
    }
);

db_test!(
    the_search_column_indexes_body_and_transcript_together,
    |f| {
        // The generated column is the contract the transcription worker relies on.
        crate::support::chat(&f.pool, "c@c.us", "dm", None).await;
        sqlx::query(
            "INSERT INTO message (chat_id, message_id, ts, body, transcript, raw) \
             VALUES ('c@c.us', 'm1', now(), 'pierogi', 'herbata', '{}'::jsonb)",
        )
        .execute(&f.pool)
        .await
        .expect("insert");
        for word in ["pierogi", "herbata"] {
            let found: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM message \
                 WHERE search @@ websearch_to_tsquery('simple', $1)",
            )
            .bind(word)
            .fetch_one(&f.pool)
            .await
            .expect("search");
            assert_eq!(found, 1, "{word}");
        }
    }
);

db_test!(a_reaction_cannot_outlive_its_message, |f| {
    crate::support::chat(&f.pool, "c@c.us", "dm", None).await;
    crate::support::said(&f.pool, "c@c.us", "m1", 0, "hello").await;
    crate::support::reaction(&f.pool, "c@c.us", "m1", "a@lid", "x", None).await;
    sqlx::query("DELETE FROM message WHERE message_id = 'm1'")
        .execute(&f.pool)
        .await
        .expect("delete");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM reaction")
        .fetch_one(&f.pool)
        .await
        .expect("count");
    assert_eq!(left, 0);
});

db_test!(an_unknown_transcript_state_is_refused, |f| {
    crate::support::chat(&f.pool, "c@c.us", "dm", None).await;
    let error = sqlx::query(
        "INSERT INTO message (chat_id, message_id, ts, raw, transcript_state) \
         VALUES ('c@c.us', 'm1', now(), '{}'::jsonb, 'maybe')",
    )
    .execute(&f.pool)
    .await
    .expect_err("the CHECK constraint must refuse this");
    // 23514 is check_violation, which is what the Python asserts by exception class.
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23514"),
        "{error}"
    );
});

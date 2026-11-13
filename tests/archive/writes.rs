//! The ingester's writers for the Rust client, against a real Postgres.
//!
//! The rules are the Python `store.py`'s, tested there in `tests/test_store_writes.py` and
//! `tests/test_revisions.py`; these check the Rust port keeps them, plus the two writes only
//! the client has: an explicit revoke and a single reaction.

use serde_json::json;
use sqlx::PgPool;
use whatsapp_core::models::MessageRow;
use whatsapp_core::store::writes;

use crate::support::{GROUP, HETTY, HETTY_LID, at, db_test};

fn row(id: &str, body: Option<&str>) -> MessageRow {
    MessageRow {
        chat_id: GROUP.into(),
        message_id: id.into(),
        ts: at(0),
        from_me: false,
        author_id: Some(HETTY_LID.into()),
        body: body.map(Into::into),
        kind: "text".into(),
        reply_to_id: None,
        ack: Some(2),
        has_media: false,
        media_mime: None,
        media_filename: None,
        media_seconds: None,
        transcript_state: "n/a".into(),
        raw: json!({"source": "test"}),
    }
}

async fn body_and_ack(pool: &PgPool, id: &str) -> (Option<String>, Option<i32>) {
    sqlx::query_as("SELECT body, ack FROM message WHERE chat_id = $1 AND message_id = $2")
        .bind(GROUP)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn revisions(pool: &PgPool, id: &str) -> Vec<(i32, Option<String>, bool)> {
    sqlx::query_as(
        "SELECT seq, text, superseded_at IS NULL FROM message_revision \
         WHERE chat_id = $1 AND message_id = $2 ORDER BY seq",
    )
    .bind(GROUP)
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

db_test!(
    a_redelivery_never_rewrites_the_body_or_lowers_the_ack,
    |f| {
        writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
        writes::upsert_message(
            &f.pool,
            &MessageRow {
                ack: Some(3),
                ..row("m1", Some("original"))
            },
        )
        .await
        .unwrap();
        writes::upsert_message(
            &f.pool,
            &MessageRow {
                ack: Some(2),
                ..row("m1", Some("changed"))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            body_and_ack(&f.pool, "m1").await,
            (Some("original".into()), Some(3))
        );
    }
);

db_test!(a_transcript_survives_a_redelivery, |f| {
    writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
    let voice = MessageRow {
        kind: "audio".into(),
        transcript_state: "pending".into(),
        ..row("v1", None)
    };
    writes::upsert_message(&f.pool, &voice).await.unwrap();
    sqlx::query("UPDATE message SET transcript = 'hello', transcript_state = 'done' WHERE message_id = 'v1'")
        .execute(&f.pool)
        .await
        .unwrap();
    writes::upsert_message(&f.pool, &voice).await.unwrap();
    let state: (Option<String>, String) =
        sqlx::query_as("SELECT transcript, transcript_state FROM message WHERE message_id = 'v1'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(state, (Some("hello".into()), "done".into()));
});

db_test!(an_edit_is_recorded_as_history_and_only_once, |f| {
    writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
    writes::upsert_message(&f.pool, &row("m1", Some("teh")))
        .await
        .unwrap();
    assert!(
        !writes::record_revision(&f.pool, GROUP, "m1", Some("teh"))
            .await
            .unwrap(),
        "same text"
    );
    assert!(
        writes::record_revision(&f.pool, GROUP, "m1", Some("the"))
            .await
            .unwrap()
    );
    assert!(
        writes::record_revision(&f.pool, GROUP, "m1", Some("the end"))
            .await
            .unwrap()
    );
    assert_eq!(
        body_and_ack(&f.pool, "m1").await.0.as_deref(),
        Some("the end")
    );
    assert_eq!(
        revisions(&f.pool, "m1").await,
        [
            (1, Some("teh".into()), false),
            (2, Some("the".into()), false),
            (3, Some("the end".into()), true)
        ]
    );
    assert!(
        !writes::record_revision(&f.pool, GROUP, "never-archived", Some("x"))
            .await
            .unwrap()
    );
});

db_test!(no_text_and_empty_text_are_not_an_edit, |f| {
    writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
    writes::upsert_message(&f.pool, &row("photo", None))
        .await
        .unwrap();
    assert!(
        !writes::record_revision(&f.pool, GROUP, "photo", Some(""))
            .await
            .unwrap()
    );
    assert!(revisions(&f.pool, "photo").await.is_empty());
});

db_test!(receipts_only_raise_the_ack, |f| {
    writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
    writes::upsert_message(
        &f.pool,
        &MessageRow {
            ack: Some(1),
            ..row("mine", Some("hi"))
        },
    )
    .await
    .unwrap();
    writes::raise_ack(&f.pool, GROUP, "mine", 3).await.unwrap();
    writes::raise_ack(&f.pool, GROUP, "mine", 2).await.unwrap();
    assert_eq!(body_and_ack(&f.pool, "mine").await.1, Some(3));
});

db_test!(a_revoke_marks_the_message_deleted_once, |f| {
    writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
    writes::upsert_message(&f.pool, &row("gone", Some("oops")))
        .await
        .unwrap();
    assert!(writes::mark_revoked(&f.pool, GROUP, "gone").await.unwrap());
    let first: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM message WHERE message_id = 'gone'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(first.is_some());
    writes::mark_revoked(&f.pool, GROUP, "gone").await.unwrap();
    let second: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM message WHERE message_id = 'gone'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(first, second, "the first deletion time stands");
    assert!(
        !writes::mark_revoked(&f.pool, GROUP, "never-archived")
            .await
            .unwrap()
    );
});

db_test!(
    a_reaction_replaces_the_previous_one_and_can_be_taken_back,
    |f| {
        writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
        writes::upsert_message(&f.pool, &row("m1", Some("news")))
            .await
            .unwrap();
        assert!(
            writes::set_reaction(&f.pool, GROUP, "m1", HETTY_LID, "👍", Some(at(5)))
                .await
                .unwrap()
        );
        assert!(
            writes::set_reaction(&f.pool, GROUP, "m1", HETTY_LID, "❤️", Some(at(6)))
                .await
                .unwrap()
        );
        let emoji: Vec<String> = sqlx::query_scalar("SELECT emoji FROM reaction")
            .fetch_all(&f.pool)
            .await
            .unwrap();
        assert_eq!(emoji, ["❤️"]);
        writes::remove_reaction(&f.pool, GROUP, "m1", HETTY_LID)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM reaction")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert!(
            !writes::set_reaction(&f.pool, GROUP, "never-archived", HETTY_LID, "👍", None)
                .await
                .unwrap(),
            "a reaction to a message the archive does not hold is skipped, not an error"
        );
    }
);

db_test!(an_author_known_by_lid_is_not_made_twice, |f| {
    sqlx::query(
        "INSERT INTO participant (participant_id, lid, contact_name) VALUES ($1, $2, 'Hetty')",
    )
    .bind(HETTY)
    .bind(HETTY_LID)
    .execute(&f.pool)
    .await
    .unwrap();
    writes::upsert_observed_participant(&f.pool, HETTY_LID, Some(HETTY_LID), Some("hettyo"))
        .await
        .unwrap();
    writes::upsert_observed_participant(&f.pool, "777@lid", Some("777@lid"), Some("new"))
        .await
        .unwrap();
    let people: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT participant_id, push_name FROM participant ORDER BY participant_id")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        people,
        [
            (HETTY.into(), Some("hettyo".into())),
            ("777@lid".into(), Some("new".into()))
        ]
    );
});

db_test!(a_success_keeps_the_last_error_visible, |f| {
    writes::set_sync_state(&f.pool, "sync", Some("boom"))
        .await
        .unwrap();
    writes::set_sync_state(&f.pool, "sync", None).await.unwrap();
    let error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM sync_state WHERE loop = 'sync'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(error.as_deref(), Some("boom"));
});

db_test!(
    an_edit_target_is_found_whatever_jid_form_its_id_was_built_from,
    |f| {
        writes::ensure_chat(&f.pool, GROUP, "group").await.unwrap();
        let ids = [
            "true_111@lid_ABC",
            "true_222@c.us_XYZ",
            "false_222@c.us_PQR_999@lid",
        ];
        for id in ids {
            writes::upsert_message(&f.pool, &row(id, Some("x")))
                .await
                .unwrap();
        }
        let resolve = |expected: &'static str, stanza: &'static str| {
            let pool = f.pool.clone();
            async move {
                writes::resolve_message_id(&pool, GROUP, expected, stanza)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            resolve("true_222@c.us_ABC", "ABC").await.as_deref(),
            Some(ids[0])
        );
        assert_eq!(
            resolve("x", "PQR").await.as_deref(),
            Some(ids[2]),
            "an id with a participant"
        );
        assert_eq!(
            resolve("x", "AB").await,
            None,
            "a prefix of an id is not that id"
        );
        assert_eq!(resolve("x", "BC").await, None, "nor a suffix");
        assert_eq!(resolve("x", "NOPE").await, None);
        // Two rows share a stanza id (sent and received): the exact id wins, either way round.
        let other = "false_111@lid_ABC_999@lid";
        writes::upsert_message(&f.pool, &row(other, Some("y")))
            .await
            .unwrap();
        assert_eq!(resolve(other, "ABC").await.as_deref(), Some(other));
        assert_eq!(resolve(ids[0], "ABC").await.as_deref(), Some(ids[0]));
    }
);

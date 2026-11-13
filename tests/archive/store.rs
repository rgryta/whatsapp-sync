//! The read queries, against a real Postgres.
//!
//! Ported from `tests/test_store_reads.py`, plus the two read-side assertions in
//! `tests/test_revisions.py`. The writers those tests call to set up are the
//! ingester's; here the rows are seeded by `support.rs` instead.

use whatsapp_core::store;

use crate::support::{
    self, DM, GROUP, HETTY, HETTY_LID, Msg, NAMELESS, at, db_test, message, said, seed,
};

/// The ids `list_chats` came back with, in the order it listed them.
fn chat_ids(rows: &[whatsapp_core::models::ChatSummary]) -> Vec<&str> {
    rows.iter().map(|row| row.chat_id.as_str()).collect()
}

fn message_ids(rows: &[whatsapp_core::models::HistoryRow]) -> Vec<&str> {
    rows.iter().map(|row| row.message_id.as_str()).collect()
}

fn hit_ids(rows: &[whatsapp_core::models::SearchHit]) -> Vec<&str> {
    rows.iter().map(|row| row.message_id.as_str()).collect()
}

async fn alias(pool: &sqlx::PgPool, alias: &str, members: &[&str]) {
    let members: Vec<String> = members.iter().map(|m| (*m).to_owned()).collect();
    store::set_alias(pool, alias, &members, None)
        .await
        .expect("set alias");
}

// ------------------------------------------------------------------------------ chats

db_test!(a_dm_takes_its_name_from_the_contact, |f| {
    // WAHA gives DM chats no name at all - 1 of 69 measured.
    seed(&f.pool).await;
    let rows = store::list_chats(&f.pool, 10, None).await.expect("chats");
    let named = |id: &str| {
        rows.iter()
            .find(|row| row.chat_id == id)
            .map(|row| row.display_name.clone())
            .unwrap_or_default()
    };
    assert_eq!(named(DM), "Henrietta Adaeze Okonkwo");
    assert_eq!(named(GROUP), "Book club regulars");
});

db_test!(an_alias_wins_over_the_contact_name, |f| {
    seed(&f.pool).await;
    store::set_alias(&f.pool, "Hetty", &[HETTY.to_owned()], Some("goes by Hetty"))
        .await
        .expect("set alias");
    let rows = store::list_chats(&f.pool, 10, None).await.expect("chats");
    let dm = rows.iter().find(|row| row.chat_id == DM).expect("the dm");
    assert_eq!(dm.display_name, "Hetty");
});

db_test!(
    a_named_row_wins_when_one_person_has_two_participant_rows,
    |f| {
        // WhatsApp hands out the same human under @c.us, @s.whatsapp.net and a lid. With an
        // unordered LIMIT 1 the nameless row can win and a known contact renders as a bare
        // jid.
        support::chat(&f.pool, DM, "dm", None).await;
        support::participant(&f.pool, support::sandy()).await;
        support::participant(
            &f.pool,
            support::Participant {
                participant_id: "447700900123@s.whatsapp.net",
                lid: None,
                phone: None,
                contact_name: None,
                push_name: None,
            },
        )
        .await;
        let rows = store::list_chats(&f.pool, 10, None).await.expect("chats");
        assert_eq!(rows[0].display_name, "Henrietta Adaeze Okonkwo");
    }
);

db_test!(a_chat_with_no_messages_is_still_listed, |f| {
    // Only `since` should drop chats; plain listing must not hide an empty one.
    // Switching to an inner join to support `since` removed 28 empty chats from the
    // live listing as a side effect.
    support::chat(&f.pool, GROUP, "group", Some("Book club regulars")).await;
    support::chat(&f.pool, DM, "dm", None).await;
    let rows = store::list_chats(&f.pool, 10, None).await.expect("chats");
    let mut ids = chat_ids(&rows);
    ids.sort();
    let mut expected = vec![GROUP, DM];
    expected.sort();
    assert_eq!(ids, expected);
});

db_test!(chats_without_since_counts_everything, |f| {
    seed(&f.pool).await;
    let rows = store::list_chats(&f.pool, 10, None).await.expect("chats");
    let group = rows.iter().find(|row| row.chat_id == GROUP).expect("group");
    assert_eq!(group.message_count, 2);
});

db_test!(chats_since_counts_only_what_is_new, |f| {
    // This is the catch-up question, answered by the tool that already lists chats.
    seed(&f.pool).await;
    let rows = store::list_chats(&f.pool, 10, Some(at(2)))
        .await
        .expect("chats");
    for id in [GROUP, DM] {
        let row = rows.iter().find(|row| row.chat_id == id).expect(id);
        assert_eq!(row.message_count, 1, "{id}");
    }
});

db_test!(chats_since_drops_chats_with_nothing_new, |f| {
    seed(&f.pool).await;
    let rows = store::list_chats(&f.pool, 10, Some(at(3)))
        .await
        .expect("chats");
    assert_eq!(chat_ids(&rows), [DM]);
});

// ---------------------------------------------------------------------------- history

db_test!(history_orders_by_ts_then_message_id, |f| {
    // WhatsApp timestamps are whole seconds, so ties are routine.
    seed(&f.pool).await;
    said(&f.pool, GROUP, "b", 90, "second").await;
    said(&f.pool, GROUP, "a", 90, "first").await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    assert_eq!(&message_ids(&rows)[rows.len() - 2..], ["a", "b"]);
});

db_test!(history_resolves_the_author_through_a_lid, |f| {
    seed(&f.pool).await;
    let rows = store::get_history(&f.pool, DM, 10, None, None)
        .await
        .expect("history");
    assert_eq!(rows[0].author.as_deref(), Some("Henrietta Adaeze Okonkwo"));
});

db_test!(an_author_resolves_across_jid_forms, |f| {
    support::chat(&f.pool, GROUP, "group", Some("Book club regulars")).await;
    support::participant(&f.pool, support::sandy()).await;
    message(
        &f.pool,
        Msg {
            message_id: "g9",
            second: 9,
            author_id: Some("447700900123@s.whatsapp.net"),
            body: Some("hi"),
            ..Msg::default()
        },
    )
    .await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    assert_eq!(rows[0].author.as_deref(), Some("Henrietta Adaeze Okonkwo"));
});

db_test!(history_windows_are_exclusive, |f| {
    seed(&f.pool).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, Some(at(1)))
        .await
        .expect("history");
    assert_eq!(message_ids(&rows), ["g2"]);
});

db_test!(history_returns_the_newest_page_in_reading_order, |f| {
    // `limit` applies to the newest end, not to an oldest-first scan. Applied the other
    // way it returns the *start* of the conversation: a 712-message chat asked for five
    // handed back five from the day it began, which reads as though the chat stopped
    // there.
    support::chat(&f.pool, DM, "dm", None).await;
    for second in 1..=7 {
        said(
            &f.pool,
            DM,
            &format!("m{second}"),
            second,
            &format!("body {second}"),
        )
        .await;
    }
    let page = store::get_history(&f.pool, DM, 3, None, None)
        .await
        .expect("history");
    let bodies: Vec<&str> = page
        .iter()
        .map(|row| row.body.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(bodies, ["body 5", "body 6", "body 7"]);
});

db_test!(
    history_with_an_after_reads_forward_from_it_and_counts_the_rest,
    |f| {
        // Asking for a period from its start used to return the END of it: 100 of
        // July's 505 messages came back as "only the 31st", with nothing to say the rest
        // existed.
        seed(&f.pool).await;
        for index in 0..5 {
            said(
                &f.pool,
                GROUP,
                &format!("p{index}"),
                10 + index,
                &format!("p{index}"),
            )
            .await;
        }
        let page = store::get_history(&f.pool, GROUP, 2, None, Some(at(9)))
            .await
            .expect("history");
        assert_eq!(message_ids(&page), ["p0", "p1"]);
        assert_eq!(
            store::count_history(&f.pool, GROUP, None, Some(at(9)))
                .await
                .expect("count"),
            5
        );
    }
);

db_test!(history_without_an_after_is_still_the_newest_page, |f| {
    seed(&f.pool).await;
    for index in 0..5 {
        said(
            &f.pool,
            GROUP,
            &format!("p{index}"),
            10 + index,
            &format!("p{index}"),
        )
        .await;
    }
    let page = store::get_history(&f.pool, GROUP, 2, None, None)
        .await
        .expect("history");
    assert_eq!(message_ids(&page), ["p3", "p4"]);
});

db_test!(history_exposes_the_ack_for_the_renderer, |f| {
    seed(&f.pool).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    // The renderer needs this column to show a receipt.
    assert_eq!(rows[0].ack, Some(2));
});

db_test!(history_shows_reactions, |f| {
    seed(&f.pool).await;
    support::reaction(&f.pool, GROUP, "g1", HETTY_LID, "\u{1f44d}", None).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    assert_eq!(
        rows[0].reactions.as_deref(),
        Some("\u{1f44d} Henrietta Adaeze Okonkwo")
    );
});

db_test!(reactions_carry_the_reacting_person, |f| {
    seed(&f.pool).await;
    support::reaction(&f.pool, GROUP, "g1", HETTY_LID, "\u{2764}\u{fe0f}", None).await;
    support::reaction(&f.pool, GROUP, "g1", NAMELESS, "\u{1f44d}", None).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    let reactions = rows[0].reactions.clone().expect("reactions");
    assert!(
        reactions.contains("Henrietta Adaeze Okonkwo"),
        "{reactions}"
    );
    assert!(reactions.contains("48999888777"), "{reactions}");
    assert!(reactions.contains('\u{2764}'), "{reactions}");
});

db_test!(an_unknown_reactor_falls_back_to_their_id, |f| {
    seed(&f.pool).await;
    support::reaction(&f.pool, GROUP, "g1", "999@lid", "\u{1f44d}", None).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    let reactions = rows[0].reactions.clone().expect("reactions");
    assert!(reactions.contains("999@lid"), "{reactions}");
});

db_test!(
    a_message_is_readable_by_id_and_absent_is_an_empty_page,
    |f| {
        seed(&f.pool).await;
        let found = store::get_message(&f.pool, GROUP, "g1")
            .await
            .expect("message");
        assert_eq!(message_ids(&found), ["g1"]);
        let missing = store::get_message(&f.pool, GROUP, "nope")
            .await
            .expect("message");
        assert!(missing.is_empty());
    }
);

// ----------------------------------------------------------------------------- people

db_test!(people_are_findable_by_any_name_form, |f| {
    seed(&f.pool).await;
    alias(&f.pool, "Hetty", &[HETTY]).await;
    for fragment in ["Hetty", "Henrietta", "hettyo", "447700"] {
        let found = store::find_people(&f.pool, fragment, 20)
            .await
            .expect("people");
        let ids: Vec<&str> = found
            .iter()
            .map(|row| row.participant_id.as_str())
            .collect();
        assert_eq!(ids, [HETTY], "{fragment}");
    }
});

db_test!(a_person_with_no_name_is_still_findable_by_number, |f| {
    seed(&f.pool).await;
    let found = store::find_people(&f.pool, "48999888", 20)
        .await
        .expect("people");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].participant_id, NAMELESS);
    assert_eq!(found[0].display_name, "48999888777");
});

db_test!(people_shows_the_alias_that_covers_them, |f| {
    seed(&f.pool).await;
    store::set_alias(&f.pool, "Hetty", &[HETTY.to_owned()], Some("note"))
        .await
        .expect("set alias");
    let rows = store::find_people(&f.pool, "Hetty", 20)
        .await
        .expect("people");
    assert_eq!(rows[0].aliases.as_deref(), Some("Hetty"));
    assert_eq!(
        rows[0].contact_name.as_deref(),
        Some("Henrietta Adaeze Okonkwo")
    );
});

db_test!(people_returns_the_chat_id_not_the_participant_id, |f| {
    // Participants are keyed @c.us; DM chats are keyed @s.whatsapp.net. Live: 69 of 69
    // DM chats use @s.whatsapp.net while 282 of 364 participants use @c.us, so a
    // participant_id handed to `messages` matches nothing at all.
    support::chat(&f.pool, "447700900123@s.whatsapp.net", "dm", None).await;
    support::participant(&f.pool, support::sandy()).await;
    let rows = store::find_people(&f.pool, "Henrietta", 20)
        .await
        .expect("people");
    assert_eq!(rows[0].participant_id, "447700900123@c.us");
    assert_eq!(
        rows[0].chat_id.as_deref(),
        Some("447700900123@s.whatsapp.net")
    );
});

db_test!(a_person_with_no_direct_chat_has_no_chat_id, |f| {
    support::participant(&f.pool, support::sandy()).await;
    let rows = store::find_people(&f.pool, "Henrietta", 20)
        .await
        .expect("people");
    assert_eq!(rows[0].chat_id, None);
});

// ----------------------------------------------------------------------------- search

db_test!(search_finds_a_word_and_snippets_it, |f| {
    seed(&f.pool).await;
    let hits = search(&f.pool, "pierogi").await;
    assert_eq!(hit_ids(&hits), ["g1"]);
    assert!(hits[0].snippet.contains("pierogi"), "{}", hits[0].snippet);
    assert_eq!(hits[0].chat_name, "Book club regulars");
});

db_test!(search_can_be_narrowed_to_a_person, |f| {
    seed(&f.pool).await;
    alias(&f.pool, "Hetty", &[HETTY]).await;
    let hits = store::search_messages(&f.pool, "pierogi", Some("Hetty"), None, None, None, 10)
        .await
        .expect("search");
    assert_eq!(hit_ids(&hits), ["g1"]);
    let none = store::search_messages(&f.pool, "pierogi", Some("nobody"), None, None, None, 10)
        .await
        .expect("search");
    assert!(none.is_empty());
});

db_test!(search_can_be_narrowed_to_a_chat, |f| {
    seed(&f.pool).await;
    let hits = store::search_messages(&f.pool, "call", None, Some(DM), None, None, 10)
        .await
        .expect("search");
    assert_eq!(hit_ids(&hits), ["d1"]);
    let none = store::search_messages(&f.pool, "call", None, Some(GROUP), None, None, 10)
        .await
        .expect("search");
    assert!(none.is_empty());
});

db_test!(search_can_be_narrowed_to_a_period, |f| {
    // Without a window, the best matches of all time crowd out the month asked about.
    seed(&f.pool).await;
    // Three weeks before the seed, which is the previous month: 2026-07-11T19:33:20Z.
    const JULY: i64 = -1_900_000;
    message(
        &f.pool,
        Msg {
            message_id: "g0",
            second: JULY,
            body: Some("pierogi in july"),
            ..Msg::default()
        },
    )
    .await;
    let windowed = store::search_messages(
        &f.pool,
        "pierogi",
        None,
        None,
        Some(at(0)),
        Some(at(JULY - 1)),
        10,
    )
    .await
    .expect("search");
    assert_eq!(hit_ids(&windowed), ["g0"]);

    let unwindowed = search(&f.pool, "pierogi").await;
    let mut ids = hit_ids(&unwindowed);
    ids.sort();
    assert_eq!(ids, ["g0", "g1"]);
});

db_test!(search_matches_a_transcript, |f| {
    seed(&f.pool).await;
    message(
        &f.pool,
        Msg {
            message_id: "v1",
            second: 4,
            kind: "audio",
            body: None,
            media_mime: Some("audio/ogg"),
            transcript: Some("mowilem o kawie"),
            transcript_state: "done",
            ..Msg::default()
        },
    )
    .await;
    let hits = search(&f.pool, "kawie").await;
    assert_eq!(hit_ids(&hits), ["v1"]);
});

db_test!(deleted_messages_are_not_searchable, |f| {
    // Their text is withheld by the default views; a snippet would leak it back.
    seed(&f.pool).await;
    said(&f.pool, DM, "m4", 5, "pineapple").await;
    assert_eq!(search(&f.pool, "pineapple").await.len(), 1);
    support::deleted(&f.pool, DM, "m4", at(6)).await;
    assert!(search(&f.pool, "pineapple").await.is_empty());
});

// -------------------------------------------------------------------------- revisions

db_test!(revisions_come_back_in_sequence_order, |f| {
    seed(&f.pool).await;
    support::revision(&f.pool, GROUP, "g1", 2, Some("edited"), Some(at(20)), None).await;
    support::revision(
        &f.pool,
        GROUP,
        "g1",
        1,
        Some("original"),
        None,
        Some(at(20)),
    )
    .await;
    let rows = store::list_revisions(&f.pool, GROUP, "g1")
        .await
        .expect("revisions");
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        [1, 2],
        "seq order, not insertion order"
    );
    assert_eq!(rows[0].first_seen_at, None, "seq 1 has no honest start");
    assert_eq!(rows[1].superseded_at, None, "seq 2 is the current text");
});

db_test!(history_reports_a_message_with_a_revision_as_edited, |f| {
    seed(&f.pool).await;
    support::revision(&f.pool, GROUP, "g1", 1, Some("was"), None, Some(at(20))).await;
    support::revision(&f.pool, GROUP, "g1", 2, Some("is"), Some(at(20)), None).await;
    let rows = store::get_history(&f.pool, GROUP, 10, None, None)
        .await
        .expect("history");
    let edited: Vec<(&str, bool)> = rows
        .iter()
        .map(|row| (row.message_id.as_str(), row.edited))
        .collect();
    assert_eq!(edited, [("g1", true), ("g2", false)]);
});

// ----------------------------------------------------------------------------- status

db_test!(status_reports_the_corpus_and_the_backlog, |f| {
    seed(&f.pool).await;
    support::sync_state(&f.pool, "sync", at(60), None, None).await;
    let rows = store::sync_status(&f.pool).await.expect("status");
    assert_eq!(rows[0].chats, 2);
    assert_eq!(rows[0].messages, 3);
    assert_eq!(rows[0].pending_transcripts, 0);
});

db_test!(status_counts_the_pending_and_the_hopeless_apart, |f| {
    seed(&f.pool).await;
    support::sync_state(&f.pool, "transcribe", at(60), None, None).await;
    message(
        &f.pool,
        Msg {
            message_id: "v1",
            second: 4,
            kind: "audio",
            body: None,
            media_mime: Some("audio/ogg"),
            transcript_state: "pending",
            ..Msg::default()
        },
    )
    .await;
    message(
        &f.pool,
        Msg {
            message_id: "v2",
            second: 5,
            kind: "audio",
            body: None,
            media_mime: Some("audio/ogg"),
            transcript_state: "failed",
            ..Msg::default()
        },
    )
    .await;
    support::deleted(&f.pool, GROUP, "g2", at(30)).await;
    support::revision(&f.pool, GROUP, "g1", 1, Some("was"), None, Some(at(20))).await;
    support::revision(&f.pool, GROUP, "g1", 2, Some("is"), Some(at(20)), None).await;
    support::watermark(&f.pool, GROUP, at(-100)).await;

    let rows = store::sync_status(&f.pool).await.expect("status");
    assert_eq!(rows[0].pending_transcripts, 1);
    assert_eq!(rows[0].failed_transcripts, 1);
    assert_eq!(rows[0].deleted, 1);
    // One message with two revisions is one edited message, not two.
    assert_eq!(rows[0].edited, 1);
    assert_eq!(rows[0].oldest_watermark, Some(at(-100)));
});

// ---------------------------------------------------------------------------- aliases

db_test!(setting_an_alias_twice_replaces_its_members, |f| {
    seed(&f.pool).await;
    alias(&f.pool, "Hetty", &[HETTY]).await;
    alias(&f.pool, "Hetty", &[NAMELESS]).await;
    let members: Vec<String> =
        sqlx::query_scalar("SELECT participant_id FROM alias_member WHERE alias = 'Hetty'")
            .fetch_all(&f.pool)
            .await
            .expect("members");
    assert_eq!(members, [NAMELESS]);
});

db_test!(deleting_an_alias_reports_whether_it_existed, |f| {
    seed(&f.pool).await;
    alias(&f.pool, "Hetty", &[HETTY]).await;
    assert!(store::delete_alias(&f.pool, "Hetty").await.expect("delete"));
    assert!(!store::delete_alias(&f.pool, "Hetty").await.expect("delete"));
});

db_test!(deleting_an_alias_takes_its_members_with_it, |f| {
    seed(&f.pool).await;
    alias(&f.pool, "Hetty", &[HETTY]).await;
    store::delete_alias(&f.pool, "Hetty").await.expect("delete");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM alias_member")
        .fetch_one(&f.pool)
        .await
        .expect("count");
    assert_eq!(left, 0);
});

// ------------------------------------------------------------------- injection guards
//
// The read queries assemble SQL from `concat!`ed fragments. Everything interpolated is
// a module-level macro; every value that came from a caller is a bind parameter, and
// sqlx's `SqlSafeStr` bound makes a query built from a `String` a compile error. These
// tests hold the line at the behavioural end, because the shape of the code invites the
// opposite assumption.

db_test!(a_search_query_cannot_execute_sql, |f| {
    seed(&f.pool).await;
    assert!(
        search(&f.pool, "'; DROP TABLE message; --")
            .await
            .is_empty()
    );
    assert_eq!(count(&f.pool, "message").await, 3);
});

db_test!(a_person_filter_cannot_execute_sql, |f| {
    seed(&f.pool).await;
    let hits = store::search_messages(
        &f.pool,
        "pierogi",
        Some("'; DELETE FROM participant; --"),
        None,
        None,
        None,
        10,
    )
    .await
    .expect("search");
    assert!(hits.is_empty());
    assert_eq!(count(&f.pool, "participant").await, 2);
});

db_test!(a_person_filter_cannot_widen_itself_into_a_tautology, |f| {
    // The ILIKE wildcards are ours; a caller supplying their own must not become
    // "match everyone".
    seed(&f.pool).await;
    let hits = store::search_messages(
        &f.pool,
        "pierogi",
        Some("%' OR '1'='1"),
        None,
        None,
        None,
        10,
    )
    .await
    .expect("search");
    assert!(hits.is_empty());
});

db_test!(a_name_fragment_cannot_execute_sql, |f| {
    seed(&f.pool).await;
    let found = store::find_people(&f.pool, "'; DROP TABLE participant; --", 20)
        .await
        .expect("people");
    assert!(found.is_empty());
    assert_eq!(count(&f.pool, "participant").await, 2);
});

db_test!(a_chat_filter_cannot_execute_sql, |f| {
    seed(&f.pool).await;
    let hits = store::search_messages(
        &f.pool,
        "pierogi",
        None,
        Some("' OR 1=1 --"),
        None,
        None,
        10,
    )
    .await
    .expect("search");
    assert!(hits.is_empty());
});

db_test!(an_alias_with_quotes_round_trips_intact, |f| {
    seed(&f.pool).await;
    let weird = "O'Brien; DROP TABLE user_alias; --";
    alias(&f.pool, weird, &[HETTY]).await;
    let stored: Vec<String> = sqlx::query_scalar("SELECT alias FROM user_alias")
        .fetch_all(&f.pool)
        .await
        .expect("aliases");
    assert_eq!(stored, [weird]);
    assert!(store::delete_alias(&f.pool, weird).await.expect("delete"));
});

// ---------------------------------------------------------------------------- helpers

async fn search(pool: &sqlx::PgPool, query: &str) -> Vec<whatsapp_core::models::SearchHit> {
    store::search_messages(pool, query, None, None, None, None, 10)
        .await
        .expect("search")
}

/// `count(*)` of one table, for the injection guards. The table name is a literal at
/// every call site; nothing here takes one from a test's input.
async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    let sql = match table {
        "message" => "SELECT count(*) FROM message",
        "participant" => "SELECT count(*) FROM participant",
        other => panic!("no counter for {other}"),
    };
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

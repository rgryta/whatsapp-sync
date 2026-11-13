//! The eight tools, rendered, against a real archive.
//!
//! This is where the store and the renderer meet, which is the only place the exact
//! bytes a client receives can be asserted. The renderer's own branches are unit-tested
//! in `crates/mcp/src/render.rs`; what is here is the whole answer to one call.
//!
//! Ported from the parts of `tests/test_mcp_render.py` and `tests/test_revisions.py`
//! that go through the server's tool functions.

use whatsapp_core::store;
use whatsapp_mcp::tools;

use crate::support::{self, DM, GROUP, HETTY, Msg, at, db_test, message, said, seed};

db_test!(chats_lists_newest_activity_first_with_counts, |f| {
    seed(&f.pool).await;
    let out = tools::chats(&f.pool, None, None).await.expect("chats");
    assert_eq!(
        out,
        "2 chat(s), newest activity first\n\
         Henrietta Adaeze Okonkwo  (dm, 1 messages, last 2026-08-01T12:00+00:00)  \
         [id 447700900123@c.us]\n\
         Book club regulars  (group, 2 messages, last 2026-08-01T12:00+00:00)  \
         [id 120363421509452248@g.us]"
    );
});

db_test!(chats_since_says_the_moment_it_counted_from, |f| {
    seed(&f.pool).await;
    let out = tools::chats(&f.pool, None, Some("2026-08-01T12:00:03Z"))
        .await
        .expect("chats");
    assert!(
        out.starts_with("1 chat(s) active since 2026-08-01T12:00+00:00\n"),
        "{out}"
    );
    assert!(out.contains("[id 447700900123@c.us]"), "{out}");
});

db_test!(chats_echoes_the_offset_it_was_given, |f| {
    // `datetime.fromisoformat` keeps the offset rather than converting to UTC, and the
    // header prints the parsed value.
    seed(&f.pool).await;
    let out = tools::chats(&f.pool, None, Some("2026-08-01T14:00:03+02:00"))
        .await
        .expect("chats");
    assert!(
        out.starts_with("1 chat(s) active since 2026-08-01T14:00+02:00"),
        "{out}"
    );
});

db_test!(an_unreadable_moment_is_a_usage_error_not_a_crash, |f| {
    seed(&f.pool).await;
    let error = tools::chats(&f.pool, None, Some("last tuesday"))
        .await
        .expect_err("a usage error");
    assert!(
        matches!(&error, tools::ToolError::Usage(message)
                 if message.contains("could not read 'last tuesday'")),
        "{error}"
    );
});

db_test!(no_chats_at_all_reads_as_no_chats, |f| {
    let out = tools::chats(&f.pool, None, None).await.expect("chats");
    assert_eq!(out, "0 chat(s), newest activity first\n(no chats)");
});

db_test!(messages_renders_a_page_oldest_first, |f| {
    seed(&f.pool).await;
    let out = tools::messages(&f.pool, GROUP, None, None, None)
        .await
        .expect("messages");
    assert_eq!(
        out,
        "2 message(s), oldest first\n\
         2026-08-01T12:00+00:00  Henrietta Adaeze Okonkwo: pierogi tonight  [id g1]\n\
         2026-08-01T12:00+00:00  48999888777: no herbata left  [id g2]"
    );
});

db_test!(a_capped_page_ends_with_the_call_for_the_next_one, |f| {
    // Without this a capped page is indistinguishable from the whole period, and a
    // caller who asked for July reads the last day of it as all of July.
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
    let forward = tools::messages(&f.pool, GROUP, Some(2), Some("2026-08-01T12:00:09Z"), None)
        .await
        .expect("messages");
    assert!(
        forward.contains("[id p0]") && forward.contains("[id p1]"),
        "{forward}"
    );
    assert!(
        forward.ends_with(
            "note: 3 more message(s) in this period after the last one shown - \
             next page: after=2026-08-01T12:00:10Z (a second of overlap, so none is skipped)"
        ),
        "{forward}"
    );

    let backward = tools::messages(&f.pool, GROUP, Some(2), None, None)
        .await
        .expect("messages");
    assert!(
        backward.ends_with(
            "note: 5 older message(s) match this request - \
             next page: before=2026-08-01T12:00:14Z (a second of overlap, so none is skipped)"
        ),
        "{backward}"
    );
});

db_test!(a_page_that_holds_everything_says_nothing_extra, |f| {
    seed(&f.pool).await;
    let out = tools::messages(&f.pool, GROUP, Some(50), None, None)
        .await
        .expect("messages");
    assert!(!out.contains("note:"), "{out}");
});

db_test!(an_empty_chat_says_nothing_rather_than_a_bare_header, |f| {
    seed(&f.pool).await;
    let out = tools::messages(&f.pool, "nobody@c.us", None, None, None)
        .await
        .expect("messages");
    assert_eq!(out, "0 message(s), oldest first\n(nothing)");
});

db_test!(a_voice_note_renders_its_transcript_and_its_duration, |f| {
    seed(&f.pool).await;
    message(
        &f.pool,
        Msg {
            chat_id: DM,
            message_id: "v1",
            second: 4,
            kind: "audio",
            body: None,
            media_mime: Some("audio/ogg; codecs=opus"),
            media_seconds: Some(14),
            transcript: Some("mowilem o kawie"),
            transcript_state: "done",
            author_id: Some(HETTY),
            ..Msg::default()
        },
    )
    .await;
    let out = tools::messages(&f.pool, DM, None, None, None)
        .await
        .expect("messages");
    assert!(
        out.ends_with(
            "2026-08-01T12:00+00:00  Henrietta Adaeze Okonkwo: \
             [audio/ogg; codecs=opus 14s] \u{201c}mowilem o kawie\u{201d}  [id v1]"
        ),
        "{out}"
    );
});

db_test!(message_reads_one_by_id, |f| {
    seed(&f.pool).await;
    let out = tools::message(&f.pool, GROUP, "g1").await.expect("message");
    assert_eq!(
        out,
        "message g1\n\
         2026-08-01T12:00+00:00  Henrietta Adaeze Okonkwo: pierogi tonight  [id g1]"
    );
});

db_test!(a_message_that_is_not_there_says_nothing, |f| {
    seed(&f.pool).await;
    let out = tools::message(&f.pool, GROUP, "nope")
        .await
        .expect("message");
    assert_eq!(out, "message nope\n(nothing)");
});

db_test!(history_marks_edited_and_withholds_deleted_text, |f| {
    seed(&f.pool).await;
    support::revision(
        &f.pool,
        GROUP,
        "g1",
        1,
        Some("pierogi tonight"),
        None,
        Some(at(20)),
    )
    .await;
    support::revision(
        &f.pool,
        GROUP,
        "g1",
        2,
        Some("pierogi tomorrow"),
        Some(at(20)),
        None,
    )
    .await;
    sqlx::query("UPDATE message SET body = 'pierogi tomorrow' WHERE message_id = 'g1'")
        .execute(&f.pool)
        .await
        .expect("apply the edit");
    support::deleted(&f.pool, GROUP, "g2", at(30)).await;

    let out = tools::messages(&f.pool, GROUP, None, None, None)
        .await
        .expect("messages");
    assert!(out.contains("(edited)"), "{out}");
    assert!(out.contains("pierogi tomorrow"), "{out}");
    assert!(out.contains("[deleted 2026-08-01T12:00+00:00]"), "{out}");
    assert!(
        !out.contains("no herbata left"),
        "the deleted text must not appear: {out}"
    );
});

db_test!(revisions_shows_the_history_and_the_deleted_text, |f| {
    // This is the only place a deleted message's text is shown. The default views
    // withhold it deliberately; asking for the history is asking for exactly this.
    seed(&f.pool).await;
    support::revision(
        &f.pool,
        GROUP,
        "g2",
        1,
        Some("no herbata left"),
        None,
        Some(at(20)),
    )
    .await;
    support::revision(
        &f.pool,
        GROUP,
        "g2",
        2,
        Some("no herbata at all"),
        Some(at(20)),
        None,
    )
    .await;
    support::deleted(&f.pool, GROUP, "g2", at(30)).await;

    let out = tools::revisions(&f.pool, GROUP, "g2")
        .await
        .expect("revisions");
    assert_eq!(
        out,
        "2 revision(s) of g2, oldest first\n  \
         1  unknown -> 2026-08-01T12:00+00:00   \"no herbata left\"\n  \
         2  2026-08-01T12:00+00:00 -> current   \"no herbata at all\"\n\
         deleted 2026-08-01T12:00+00:00\n\
         note: revision 1 is the earliest text this archive saw, which is not \
         necessarily what was first sent - a message edited before it was first read \
         arrives already changed."
    );
});

db_test!(revisions_of_an_unchanged_message_says_so, |f| {
    seed(&f.pool).await;
    let out = tools::revisions(&f.pool, GROUP, "g1")
        .await
        .expect("revisions");
    assert_eq!(
        out,
        "0 revision(s) of g1, oldest first\nno recorded changes"
    );
});

db_test!(
    revisions_of_a_message_the_archive_lacks_says_which_one,
    |f| {
        seed(&f.pool).await;
        let out = tools::revisions(&f.pool, GROUP, "nope")
            .await
            .expect("revisions");
        assert_eq!(out, "no message nope in chat 120363421509452248@g.us");
    }
);

db_test!(search_renders_the_chat_the_author_and_a_snippet, |f| {
    seed(&f.pool).await;
    let out = tools::search(&f.pool, "pierogi", None, None, None, None, None)
        .await
        .expect("search");
    assert_eq!(
        out,
        "1 match(es) for 'pierogi'\n\
         2026-08-01T12:00+00:00  Book club regulars  Henrietta Adaeze Okonkwo: \
         <b>pierogi</b> tonight   [chat 120363421509452248@g.us id g1]"
    );
});

db_test!(a_search_that_matches_nothing_says_so, |f| {
    seed(&f.pool).await;
    let out = tools::search(&f.pool, "zeppelin", None, None, None, None, None)
        .await
        .expect("search");
    assert_eq!(out, "0 match(es) for 'zeppelin'\n(no matches)");
});

db_test!(
    a_query_with_an_apostrophe_is_quoted_the_way_python_prints_it,
    |f| {
        seed(&f.pool).await;
        let out = tools::search(&f.pool, "don't", None, None, None, None, None)
            .await
            .expect("search");
        assert!(out.starts_with("0 match(es) for \"don't\"\n"), "{out}");
    }
);

db_test!(people_shows_every_name_form_and_a_usable_chat_id, |f| {
    seed(&f.pool).await;
    store::set_alias(&f.pool, "Hetty", &[HETTY.to_owned()], None)
        .await
        .expect("set alias");
    let out = tools::people(&f.pool, "Henrietta").await.expect("people");
    assert_eq!(
        out,
        "1 person(s)\n\
         Hetty  (Henrietta Adaeze Okonkwo, hettyo, Hetty; 447700900123)  \
         [chat 447700900123@c.us]"
    );
});

db_test!(people_with_no_match_says_so, |f| {
    seed(&f.pool).await;
    let out = tools::people(&f.pool, "Zbigniew").await.expect("people");
    assert_eq!(out, "no matching people");
});

db_test!(status_reports_the_corpus_and_the_window, |f| {
    seed(&f.pool).await;
    support::sync_state(&f.pool, "sync", at(60), None, None).await;
    support::watermark(&f.pool, GROUP, at(-100)).await;
    let out = tools::status(&f.pool).await.expect("status");
    assert_eq!(
        out,
        "sync: last run 2026-08-01T12:01+00:00\n\
         2 chats, 3 messages, oldest watermark 2026-08-01T11:58+00:00, \
         0 transcript(s) pending\n\
         0 message(s) edited, 0 deleted; edits and deletions are recorded as they arrive, \
         so any made before this client started archiving are unknown rather than absent"
    );
});

db_test!(status_before_the_ingester_has_ever_run, |f| {
    let out = tools::status(&f.pool).await.expect("status");
    assert_eq!(out, "the ingester has never run");
});

db_test!(
    status_calls_a_live_fault_an_error_and_a_healed_one_recovered,
    |f| {
        seed(&f.pool).await;
        // A failed run stamps both columns from one `now()`.
        support::sync_state(
            &f.pool,
            "sync",
            at(60),
            Some("2 chat(s) failed"),
            Some(at(60)),
        )
        .await;
        // A good run since then moved only `last_run_at`.
        support::sync_state(
            &f.pool,
            "transcribe",
            at(600),
            Some("whisper fell over"),
            Some(at(60)),
        )
        .await;
        let out = tools::status(&f.pool).await.expect("status");
        assert!(
            out.contains("sync: last run 2026-08-01T12:01+00:00  ERROR 2 chat(s) failed at"),
            "{out}"
        );
        assert!(
            out.contains("transcribe: last run 2026-08-01T12:10+00:00  recovered; last failed"),
            "{out}"
        );
    }
);

db_test!(set_alias_covers_the_members_it_was_given, |f| {
    seed(&f.pool).await;
    let out = tools::set_alias(&f.pool, "Hetty", &[HETTY.to_owned()], Some("goes by Hetty"))
        .await
        .expect("set alias");
    assert_eq!(out, "alias 'Hetty' now covers 1 participant(s)");
    // And it wins over the address-book name everywhere.
    let chats = tools::chats(&f.pool, None, None).await.expect("chats");
    assert!(chats.contains("Hetty  (dm,"), "{chats}");
});

db_test!(
    set_alias_with_no_members_deletes_it_and_says_whether_it_was_there,
    |f| {
        seed(&f.pool).await;
        tools::set_alias(&f.pool, "Hetty", &[HETTY.to_owned()], None)
            .await
            .expect("set alias");
        assert_eq!(
            tools::set_alias(&f.pool, "Hetty", &[], None)
                .await
                .expect("delete"),
            "alias 'Hetty' removed"
        );
        assert_eq!(
            tools::set_alias(&f.pool, "Hetty", &[], None)
                .await
                .expect("delete"),
            "alias 'Hetty' did not exist"
        );
    }
);

db_test!(a_limit_above_the_cap_is_clamped_not_refused, |f| {
    seed(&f.pool).await;
    // 10_000 would be a page nobody can read and a query nobody meant to run.
    let out = tools::messages(&f.pool, GROUP, Some(10_000), None, None)
        .await
        .expect("messages");
    assert!(out.starts_with("2 message(s), oldest first"), "{out}");
});

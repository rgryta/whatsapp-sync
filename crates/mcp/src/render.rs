//! Archive rows to text a model can read.
//!
//! ISO 8601 UTC everywhere, so a timestamp here is directly comparable with one from
//! any other tool. Media is described, never fetched.
//!
//! This is the rendering rule, not a storage rule: the archive keeps whole payloads and
//! this layer decides what a reader actually sees.
//!
//! Every function here is a line-for-line port of `whatsapp_sync/mcp/render.py`, down
//! to the double spaces and the typographic quotes around a transcript, because the two
//! servers answer the same questions for as long as both are deployed and a reader
//! should not be able to tell which one replied.

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use whatsapp_core::models::{ChatSummary, HistoryRow, Person, Revision, SearchHit, StatusRow};

/// ISO 8601 UTC to the minute, or `?` for an absent time.
pub fn when(moment: Option<DateTime<Utc>>) -> String {
    match moment {
        Some(moment) => moment.format("%Y-%m-%dT%H:%M+00:00").to_string(),
        None => "?".to_owned(),
    }
}

/// Python's truthiness on a `str | None` column: NULL and `''` are the same absence.
fn text(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// WAHA's ack ladder. Only meaningful on messages you sent: on received ones it is -1
/// on 4,983 of 5,680 rows, so showing it there would be noise dressed as information.
fn receipt(row: &HistoryRow) -> Option<&'static str> {
    if !row.from_me {
        return None;
    }
    match row.ack {
        Some(1) => Some("sent"),
        Some(2) => Some("delivered"),
        Some(3) => Some("read"),
        Some(4) => Some("played"),
        _ => None,
    }
}

fn media(row: &HistoryRow) -> String {
    let mut parts = vec![
        row.media_mime
            .clone()
            .unwrap_or_else(|| "attachment".into()),
    ];
    if let Some(filename) = text(&row.media_filename) {
        parts.push(format!("\"{filename}\""));
    }
    // Zero seconds is no duration, exactly as a falsy 0 is in the Python.
    if let Some(seconds) = row.media_seconds.filter(|seconds| *seconds != 0) {
        parts.push(format!("{seconds}s"));
    }
    format!("[{}]", parts.join(" "))
}

fn who(from_me: bool, author: &Option<String>) -> &str {
    if from_me {
        "me"
    } else {
        text(author).unwrap_or("?")
    }
}

pub fn render_history(rows: &[HistoryRow], header: &str) -> String {
    if rows.is_empty() {
        return format!("{header}\n(nothing)");
    }
    let mut lines = vec![header.to_owned()];
    for row in rows {
        let mut bits = vec![format!(
            "{}  {}:",
            when(Some(row.ts)),
            who(row.from_me, &row.author)
        )];
        if let Some(deleted) = row.deleted_at {
            // The text stays in the archive and stays in the revision log; this layer
            // withholds it. Showing what someone deleted alongside the note that they
            // deleted it would make the marker pointless.
            bits.push(format!("[deleted {}]", when(Some(deleted))));
            lines.push(format!("{}  [id {}]", bits.join(" "), row.message_id));
            continue;
        }
        if row.kind != "text" {
            bits.push(media(row));
        }
        if let Some(body) = text(&row.body) {
            bits.push(body.to_owned());
        }
        if row.edited {
            bits.push("(edited)".to_owned());
        }
        if let Some(transcript) = text(&row.transcript) {
            // Quoted so a transcript is never mistaken for something typed.
            bits.push(format!("\u{201c}{transcript}\u{201d}"));
        }
        if let Some(reply_to) = text(&row.reply_to_id) {
            bits.push(format!("(reply to {reply_to})"));
        }
        if let Some(reactions) = text(&row.reactions) {
            // "emoji Name, emoji Name" - who reacted, not just what was left behind.
            bits.push(format!("[{reactions}]"));
        }
        if let Some(receipt) = receipt(row) {
            bits.push(format!("({receipt})"));
        }
        lines.push(format!("{}  [id {}]", bits.join(" "), row.message_id));
    }
    lines.join("\n")
}

pub fn render_chats(rows: &[ChatSummary], header: &str) -> String {
    if rows.is_empty() {
        return format!("{header}\n(no chats)");
    }
    let mut lines = vec![header.to_owned()];
    for row in rows {
        lines.push(format!(
            "{}  ({}, {} messages, last {})  [id {}]",
            row.display_name,
            row.kind,
            row.message_count,
            when(row.last_ts),
            row.chat_id,
        ));
    }
    lines.join("\n")
}

pub fn render_search(rows: &[SearchHit], header: &str) -> String {
    if rows.is_empty() {
        return format!("{header}\n(no matches)");
    }
    let mut lines = vec![header.to_owned()];
    for row in rows {
        lines.push(format!(
            "{}  {}  {}: {}  [chat {} id {}]",
            when(Some(row.ts)),
            row.chat_name,
            who(row.from_me, &row.author),
            row.snippet,
            row.chat_id,
            row.message_id,
        ));
    }
    lines.join("\n")
}

/// Every name form is shown on purpose.
///
/// Printing one resolved name would hide that two people matched the fragment, and the
/// caller could not tell which one they got.
pub fn render_people(rows: &[Person]) -> String {
    if rows.is_empty() {
        return "no matching people".to_owned();
    }
    let mut lines = vec![format!("{} person(s)", rows.len())];
    for row in rows {
        let known: Vec<&str> = [&row.contact_name, &row.push_name, &row.aliases]
            .into_iter()
            .filter_map(text)
            .collect();
        // The chat id is what a caller actually needs next; the participant id is in a
        // different namespace and will not match a chat.
        let location = match text(&row.chat_id) {
            Some(chat_id) => format!("chat {chat_id}"),
            None => "no direct chat".to_owned(),
        };
        let names = if known.is_empty() {
            "no name".to_owned()
        } else {
            known.join(", ")
        };
        lines.push(format!(
            "{}  ({names}; {})  [{location}]",
            row.display_name,
            text(&row.phone).unwrap_or("no number"),
        ));
    }
    lines.join("\n")
}

/// Say whether the recorded fault is happening now or is only history.
///
/// `set_sync_state` keeps the last error after a good run on purpose, so that a fault
/// recurring every other cycle cannot hide between two green ones. What was missing was
/// the other half: nothing distinguished a loop that is failing from one that failed
/// twelve days ago and has run fine every cycle since, so `status` went on printing
/// ERROR while the ingester was demonstrably healthy.
///
/// A failed run writes `last_run_at` and `last_error_at` from the same `now()`, so they
/// are equal; a good run moves only `last_run_at`. That ordering is the whole test --
/// no extra column, and it reads correctly for rows written before this.
fn fault(row: &StatusRow) -> String {
    let error = row.last_error.as_deref().unwrap_or("None");
    match (row.last_error_at, row.last_run_at) {
        (Some(stamped), Some(ran)) if stamped < ran => {
            format!("  recovered; last failed {}: {error}", when(Some(stamped)))
        }
        (stamped, _) => format!("  ERROR {error} at {}", when(stamped)),
    }
}

pub fn render_status(rows: &[StatusRow]) -> String {
    let Some(first) = rows.first() else {
        return "the ingester has never run".to_owned();
    };
    let mut lines = Vec::with_capacity(rows.len() + 2);
    for row in rows {
        let mut line = format!("{}: last run {}", row.loop_name, when(row.last_run_at));
        if text(&row.last_error).is_some() {
            line.push_str(&fault(row));
        }
        lines.push(line);
    }
    let mut transcripts = format!("{} transcript(s) pending", first.pending_transcripts);
    if first.failed_transcripts != 0 {
        // Without this, "0 transcript(s) pending" is read as "every voice note is
        // searchable". These are permanent: WhatsApp no longer serves the audio, so
        // there is nothing queued and nothing coming.
        transcripts.push_str(&format!(
            ", {} voice note(s) permanently untranscribed \
             (WhatsApp no longer holds the audio)",
            first.failed_transcripts
        ));
    }
    lines.push(format!(
        "{} chats, {} messages, oldest watermark {}, {transcripts}",
        first.chats,
        first.messages,
        when(first.oldest_watermark),
    ));
    // Say the limit out loud. Without it a reader has no way to know that "no edits"
    // means "none since the client started" rather than "none ever", and would state the
    // stronger claim.
    lines.push(format!(
        "{} message(s) edited, {} deleted; edits and deletions are recorded as they \
         arrive, so any made before this client started archiving are unknown rather \
         than absent",
        first.edited, first.deleted,
    ));
    lines.join("\n")
}

/// One message's text over time, oldest first.
///
/// This is the only place a deleted message's text is shown. The default views withhold
/// it deliberately; asking for the history is asking for exactly this.
pub fn render_revisions(
    rows: &[Revision],
    header: &str,
    deleted_at: Option<DateTime<Utc>>,
) -> String {
    if rows.is_empty() && deleted_at.is_none() {
        return format!("{header}\nno recorded changes");
    }
    let mut lines = vec![header.to_owned()];
    for row in rows {
        // A NULL start is not a missing value: nothing ever recorded when a message was
        // first stored, so the earliest text we hold genuinely has no start time.
        let start = match row.first_seen_at {
            Some(moment) => when(Some(moment)),
            None => "unknown".to_owned(),
        };
        let end = match row.superseded_at {
            Some(moment) => when(Some(moment)),
            None => "current".to_owned(),
        };
        let body = text(&row.text).unwrap_or("(no text)");
        lines.push(format!("  {}  {start} -> {end}   \"{body}\"", row.seq));
    }
    if let Some(deleted) = deleted_at {
        lines.push(format!("deleted {}", when(Some(deleted))));
    }
    if !rows.is_empty() {
        lines.push(
            "note: revision 1 is the earliest text this archive saw, which is not \
             necessarily what was first sent - a message edited before it was first \
             read arrives already changed."
                .to_owned(),
        );
    }
    lines.join("\n")
}

/// Say how much of the requested period was not shown, and the call that fetches it.
///
/// Without this a capped page is indistinguishable from the whole period, and a caller
/// who asked for July reads the last day of it as all of July. The cursor overlaps the
/// page by a second because timestamps are whole seconds and both bounds are exclusive:
/// stepping from the last row's own second would skip any message that shares it.
pub fn window_note(
    more: i64,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    oldest_first: bool,
) -> Option<String> {
    if more <= 0 {
        return None;
    }
    let second = TimeDelta::seconds(1);
    if oldest_first {
        let cursor = (last - second).to_rfc3339_opts(SecondsFormat::Secs, true);
        return Some(format!(
            "note: {more} more message(s) in this period after the last one shown - \
             next page: after={cursor} (a second of overlap, so none is skipped)"
        ));
    }
    let cursor = (first + second).to_rfc3339_opts(SecondsFormat::Secs, true);
    Some(format!(
        "note: {more} older message(s) match this request - \
         next page: before={cursor} (a second of overlap, so none is skipped)"
    ))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn history_row() -> HistoryRow {
        HistoryRow {
            chat_id: "g@g.us".to_owned(),
            message_id: "m1".to_owned(),
            ts: Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 1).unwrap(),
            from_me: false,
            kind: "text".to_owned(),
            body: Some("pierogi".to_owned()),
            transcript: None,
            media_mime: None,
            media_filename: None,
            media_seconds: None,
            reply_to_id: None,
            ack: None,
            deleted_at: None,
            edited: false,
            author: Some("Hetty".to_owned()),
            reactions: None,
        }
    }

    #[test]
    fn history_renders_iso_utc_and_the_author_name() {
        let text = render_history(&[history_row()], "1 message");
        assert!(text.contains("2026-08-01T12:00"), "{text}");
        assert!(text.contains("Hetty"), "{text}");
        assert!(text.contains("pierogi"), "{text}");
        assert!(text.contains("[id m1]"), "{text}");
    }

    #[test]
    fn my_own_messages_say_me() {
        let text = render_history(
            &[HistoryRow {
                from_me: true,
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains(" me:"), "{text}");
    }

    #[test]
    fn a_voice_note_shows_its_transcript_not_its_bytes() {
        let text = render_history(
            &[HistoryRow {
                kind: "audio".to_owned(),
                body: None,
                transcript: Some("spoken words".to_owned()),
                media_mime: Some("audio/ogg".to_owned()),
                media_seconds: Some(14),
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains("spoken words"), "{text}");
        assert!(text.contains("14s"), "{text}");
        assert!(text.contains("audio/ogg"), "{text}");
    }

    #[test]
    fn media_is_described_never_downloaded() {
        let text = render_history(
            &[HistoryRow {
                kind: "image".to_owned(),
                body: Some("a caption".to_owned()),
                media_mime: Some("image/jpeg".to_owned()),
                media_filename: Some("beach.jpg".to_owned()),
                from_me: true,
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains("[image/jpeg"), "{text}");
        assert!(text.contains("beach.jpg"), "{text}");
        assert!(text.contains("a caption"), "{text}");
        assert!(!text.contains("http"), "{text}");
    }

    #[test]
    fn a_text_message_is_not_prefixed_with_its_own_type() {
        // The sibling project's renderer prefixed every line with `[TEXT]`, which is
        // one wasted token per message and tells a reader nothing.
        let text = render_history(&[history_row()], "h");
        assert_eq!(text, "h\n2026-08-01T12:00+00:00  Hetty: pierogi  [id m1]");
    }

    #[test]
    fn reactions_and_replies_are_shown() {
        let text = render_history(
            &[HistoryRow {
                reactions: Some("\u{1f44d}".to_owned()),
                reply_to_id: Some("ABC123".to_owned()),
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains('\u{1f44d}'), "{text}");
        assert!(text.contains("ABC123"), "{text}");
    }

    #[test]
    fn an_empty_history_says_so_rather_than_rendering_a_bare_header() {
        assert!(render_history(&[], "0 messages").contains("(nothing)"));
    }

    #[test]
    fn a_read_receipt_shows_only_on_my_own_messages() {
        // On received messages WAHA reports -1 for 4,983 of 5,680 rows. Rendering that
        // would be noise dressed as information.
        let mine = render_history(
            &[HistoryRow {
                from_me: true,
                ack: Some(3),
                ..history_row()
            }],
            "h",
        );
        assert!(mine.contains("(read)"), "{mine}");
        let theirs = render_history(
            &[HistoryRow {
                ack: Some(3),
                ..history_row()
            }],
            "h",
        );
        assert!(!theirs.contains("(read)"), "{theirs}");
    }

    #[test]
    fn an_unknown_ack_is_not_rendered() {
        let text = render_history(
            &[HistoryRow {
                from_me: true,
                ack: Some(-1),
                ..history_row()
            }],
            "h",
        );
        assert!(!text.contains('('), "{text}");
    }

    #[test]
    fn reactions_name_who_left_them() {
        // 513 of 1,252 reacted messages carry more than one reaction.
        let text = render_history(
            &[HistoryRow {
                reactions: Some("\u{2764}\u{fe0f} Wanda, \u{1f44d} Bogdan".to_owned()),
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains("Wanda") && text.contains("Bogdan"), "{text}");
    }

    #[test]
    fn an_unattributed_message_renders_a_question_mark() {
        let text = render_history(
            &[HistoryRow {
                author: None,
                ..history_row()
            }],
            "h",
        );
        assert!(text.contains(" ?:"), "{text}");
    }

    #[test]
    fn chats_render_counts_and_last_activity() {
        let text = render_chats(
            &[ChatSummary {
                chat_id: "g@g.us".to_owned(),
                kind: "group".to_owned(),
                display_name: "Book club".to_owned(),
                last_ts: Some(Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()),
                message_count: 42,
            }],
            "1 chat",
        );
        assert!(text.contains("Book club"), "{text}");
        assert!(text.contains("42 messages"), "{text}");
        assert!(text.contains("[id g@g.us]"), "{text}");
    }

    #[test]
    fn an_empty_chat_list_says_so() {
        assert!(render_chats(&[], "0 chats").contains("(no chats)"));
    }

    #[test]
    fn search_shows_the_chat_and_a_snippet() {
        let text = render_search(
            &[SearchHit {
                chat_id: "g@g.us".to_owned(),
                message_id: "m1".to_owned(),
                ts: Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap(),
                from_me: false,
                chat_name: "Book club".to_owned(),
                author: Some("Hetty".to_owned()),
                snippet: "a <b>pierogi</b> here".to_owned(),
            }],
            "1 match",
        );
        assert!(text.contains("Book club"), "{text}");
        assert!(text.contains("pierogi"), "{text}");
        assert!(text.contains("chat g@g.us"), "{text}");
    }

    #[test]
    fn no_matches_reads_as_no_matches() {
        assert!(render_search(&[], "0 matches").contains("(no matches)"));
    }

    fn person() -> Person {
        Person {
            participant_id: "447700900123@c.us".to_owned(),
            lid: Some("269@lid".to_owned()),
            phone: Some("447700900123".to_owned()),
            contact_name: Some("Henrietta Adaeze Okonkwo".to_owned()),
            push_name: Some("hettyo".to_owned()),
            display_name: "Hetty".to_owned(),
            aliases: Some("Hetty".to_owned()),
            chat_id: Some("447700900123@s.whatsapp.net".to_owned()),
        }
    }

    #[test]
    fn people_shows_every_name_form_so_ambiguity_is_visible() {
        let text = render_people(&[person()]);
        assert!(text.contains("Hetty"), "{text}");
        assert!(text.contains("Henrietta Adaeze Okonkwo"), "{text}");
        assert!(text.contains("hettyo"), "{text}");
    }

    #[test]
    fn a_person_with_no_names_still_renders() {
        let text = render_people(&[Person {
            lid: None,
            phone: None,
            contact_name: None,
            push_name: None,
            display_name: "48999@c.us".to_owned(),
            aliases: None,
            chat_id: None,
            ..person()
        }]);
        assert!(text.contains("no name"), "{text}");
        assert!(text.contains("no number"), "{text}");
    }

    #[test]
    fn people_hands_back_a_usable_chat_id() {
        // Participants are keyed @c.us, DM chats @s.whatsapp.net. A participant_id
        // passed to `messages` matches nothing, so the chat id is what the caller gets.
        let text = render_people(&[Person {
            lid: None,
            contact_name: Some("Henrietta".to_owned()),
            push_name: None,
            ..person()
        }]);
        assert!(text.contains("chat 447700900123@s.whatsapp.net"), "{text}");
        assert!(!text.contains("447700900123@c.us"), "{text}");
    }

    #[test]
    fn a_person_with_no_direct_chat_says_so() {
        let text = render_people(&[Person {
            chat_id: None,
            ..person()
        }]);
        assert!(text.contains("no direct chat"), "{text}");
    }

    #[test]
    fn no_match_reads_as_no_match() {
        assert_eq!(render_people(&[]), "no matching people");
    }

    /// A failing run stamps both columns from one `now()`, so they match by default.
    fn status_row() -> StatusRow {
        let failed_at = Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap();
        StatusRow {
            loop_name: "sync".to_owned(),
            last_run_at: Some(failed_at),
            last_error: Some("2 chat(s) failed".to_owned()),
            last_error_at: Some(failed_at),
            chats: 118,
            messages: 20000,
            oldest_watermark: Some(Utc.with_ymd_and_hms(2026, 5, 2, 0, 0, 0).unwrap()),
            pending_transcripts: 4,
            failed_transcripts: 0,
            deleted: 0,
            edited: 0,
        }
    }

    #[test]
    fn status_says_when_voice_notes_can_never_be_transcribed() {
        let text = render_status(&[StatusRow {
            pending_transcripts: 0,
            failed_transcripts: 65,
            ..status_row()
        }]);
        assert!(text.contains("0 transcript(s) pending"), "{text}");
        assert!(
            text.contains("65 voice note(s) permanently untranscribed"),
            "{text}"
        );
        assert!(text.contains("no longer holds the audio"), "{text}");
    }

    #[test]
    fn status_stays_quiet_when_nothing_failed() {
        let text = render_status(&[status_row()]);
        assert!(!text.contains("permanently untranscribed"), "{text}");
    }

    #[test]
    fn status_surfaces_an_error_rather_than_burying_it() {
        let text = render_status(&[status_row()]);
        assert!(text.contains("ERROR"), "{text}");
        assert!(text.contains("2 chat(s) failed"), "{text}");
        assert!(text.contains("118 chats"), "{text}");
        assert!(text.contains("4 transcript(s) pending"), "{text}");
    }

    #[test]
    fn status_does_not_call_a_fault_current_once_a_run_has_succeeded() {
        // The error column is kept after a good run on purpose, so a fault that recurs
        // every other cycle cannot hide. Reading that as a live fault is the other
        // mistake: a WAHA outage twelve days ago had `status` shouting ERROR at a
        // healthy ingester that had run every cycle since.
        let text = render_status(&[StatusRow {
            last_run_at: Some(Utc.with_ymd_and_hms(2026, 8, 13, 9, 0, 0).unwrap()),
            ..status_row()
        }]);
        assert!(!text.contains("ERROR"), "{text}");
        assert!(text.contains("recovered"), "{text}");
        assert!(text.contains("2 chat(s) failed"), "{text}");
    }

    #[test]
    fn status_treats_a_fault_stamped_with_the_last_run_as_current() {
        let text = render_status(&[StatusRow {
            last_error_at: None,
            ..status_row()
        }]);
        assert!(text.contains("ERROR"), "{text}");
    }

    #[test]
    fn status_with_no_rows_says_the_ingester_never_ran() {
        assert!(render_status(&[]).contains("never run"));
    }

    #[test]
    fn status_says_the_edit_limit_out_loud() {
        let text = render_status(&[StatusRow {
            edited: 3,
            deleted: 2,
            ..status_row()
        }]);
        assert!(text.contains("3 message(s) edited, 2 deleted"), "{text}");
        assert!(text.contains("recorded as they arrive"), "{text}");
    }

    fn revision(seq: i32, body: &str) -> Revision {
        Revision {
            seq,
            text: Some(body.to_owned()),
            first_seen_at: None,
            superseded_at: None,
        }
    }

    #[test]
    fn revisions_render_is_not_an_error_when_there_is_no_history() {
        assert!(render_revisions(&[], "h", None).contains("no recorded changes"));
    }

    #[test]
    fn the_first_revision_has_no_honest_start_time() {
        let text = render_revisions(&[revision(1, "hello")], "h", None);
        assert!(text.contains("unknown -> current"), "{text}");
        assert!(
            text.contains("not necessarily what was first sent"),
            "{text}"
        );
    }

    #[test]
    fn a_revision_without_text_says_so_rather_than_printing_nothing() {
        let text = render_revisions(
            &[Revision {
                text: None,
                ..revision(2, "")
            }],
            "h",
            None,
        );
        assert!(text.contains("(no text)"), "{text}");
    }

    #[test]
    fn a_deleted_message_reports_its_deletion_even_with_no_revisions() {
        let text = render_revisions(
            &[],
            "h",
            Some(Utc.with_ymd_and_hms(2026, 8, 2, 7, 30, 0).unwrap()),
        );
        assert!(text.contains("deleted 2026-08-02T07:30+00:00"), "{text}");
        // The closing note is about revision 1, and there is none.
        assert!(!text.contains("not necessarily"), "{text}");
    }

    #[test]
    fn a_capped_page_says_how_many_more_the_period_holds() {
        let first = Utc.with_ymd_and_hms(2026, 7, 22, 11, 55, 2).unwrap();
        let last = Utc.with_ymd_and_hms(2026, 7, 26, 9, 0, 0).unwrap();

        let forward = window_note(405, first, last, true).expect("a note");
        let backward = window_note(405, first, last, false).expect("a note");

        assert!(forward.contains("405 more message(s)"), "{forward}");
        assert!(forward.contains("after=2026-07-26T08:59:59Z"), "{forward}");
        assert!(backward.contains("405 older message(s)"), "{backward}");
        assert!(
            backward.contains("before=2026-07-22T11:55:03Z"),
            "{backward}"
        );
        assert!(window_note(0, first, last, true).is_none());
    }
}

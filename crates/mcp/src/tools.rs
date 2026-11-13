//! The eight archive tools, as plain async functions over a pool.
//!
//! Keeping them out of the server object is what lets the tests call them directly, and
//! keeps the MCP wiring to registration. Nothing here holds SQL — every statement lives
//! in `whatsapp_core::store` — and nothing in this crate depends on `whatsapp-client`, so
//! the linker makes reaching WhatsApp impossible rather than merely discouraged.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use sqlx::postgres::PgPool;
use whatsapp_core::store;

use crate::render;

pub const MAX_LIMIT: i64 = 200;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller's fault, and reported back to them as text rather than logged.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, ToolError>;

/// A caller asking for 10_000 gets 200, not an error and not 10_000.
///
/// The default differs per tool, as it does in the Python: 50 for the two paging tools,
/// 25 for search.
pub fn clamp(limit: Option<i64>, default: i64) -> i64 {
    match limit {
        Some(limit) if limit >= 1 => limit.min(MAX_LIMIT),
        _ => default,
    }
}

/// ISO 8601 or an epoch. What this server prints, it also accepts.
///
/// The offset is kept rather than normalised to UTC because `chats` echoes the parsed
/// moment back in its header, and `datetime.fromisoformat` keeps whatever offset it was
/// handed. A value with no offset is read as UTC, which is what
/// `parsed.replace(tzinfo=UTC)` does — note that this is *not* the server's local
/// timezone, even though the container sets `TZ`.
pub fn parse_moment(value: Option<&str>) -> Result<Option<DateTime<FixedOffset>>> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalised = value.replace('Z', "+00:00");
    if let Ok(moment) = DateTime::parse_from_rfc3339(&normalised) {
        return Ok(Some(moment));
    }
    // `+0200` as well as `+02:00`, which `fromisoformat` also accepts.
    for format in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%z"] {
        if let Ok(moment) = DateTime::parse_from_str(&normalised, format) {
            return Ok(Some(moment));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Some(as_utc(naive)));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(Some(as_utc(
            date.and_hms_opt(0, 0, 0).expect("midnight exists"),
        )));
    }
    // An epoch, which is what WAHA's own payloads carry.
    if let Ok(epoch) = value.parse::<f64>() {
        let seconds = epoch.trunc() as i64;
        let nanos = ((epoch - epoch.trunc()) * 1e9).round() as u32;
        if let Some(moment) = DateTime::from_timestamp(seconds, nanos) {
            return Ok(Some(moment.fixed_offset()));
        }
    }
    Err(ToolError::Usage(format!(
        "could not read {} as a time",
        py_repr(value)
    )))
}

fn as_utc(naive: NaiveDateTime) -> DateTime<FixedOffset> {
    Utc.from_utc_datetime(&naive).fixed_offset()
}

fn utc(moment: Option<DateTime<FixedOffset>>) -> Option<DateTime<Utc>> {
    moment.map(|moment| moment.with_timezone(&Utc))
}

/// `datetime.isoformat(timespec="minutes")`, offset and all.
fn iso_minutes(moment: DateTime<FixedOffset>) -> String {
    moment.format("%Y-%m-%dT%H:%M%:z").to_string()
}

/// Python's `repr` of a string, which is what the headers print a caller's value with.
///
/// shortcut: control characters are passed through rather than escaped as `\xNN`, so a
/// query containing one renders differently from the Python. Upgrade this if a header
/// ever has to be byte-identical for such a value; no caller sends one today.
fn py_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

// ----------------------------------------------------------------------------- tools

pub async fn chats(pool: &PgPool, limit: Option<i64>, since: Option<&str>) -> Result<String> {
    let moment = parse_moment(since)?;
    let rows = store::list_chats(pool, clamp(limit, 50), utc(moment)).await?;
    let header = match moment {
        Some(moment) => format!(
            "{} chat(s) active since {}",
            rows.len(),
            iso_minutes(moment)
        ),
        None => format!("{} chat(s), newest activity first", rows.len()),
    };
    Ok(render::render_chats(&rows, &header))
}

pub async fn messages(
    pool: &PgPool,
    chat_id: &str,
    limit: Option<i64>,
    after: Option<&str>,
    before: Option<&str>,
) -> Result<String> {
    let after = parse_moment(after)?;
    let before = parse_moment(before)?;
    let rows = store::get_history(pool, chat_id, clamp(limit, 50), utc(before), utc(after)).await?;
    let out = render::render_history(&rows, &format!("{} message(s), oldest first", rows.len()));
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Ok(out);
    };
    let total = store::count_history(pool, chat_id, utc(before), utc(after)).await?;
    match render::window_note(
        total - rows.len() as i64,
        first.ts,
        last.ts,
        after.is_some(),
    ) {
        Some(note) => Ok(format!("{out}\n{note}")),
        None => Ok(out),
    }
}

pub async fn message(pool: &PgPool, chat_id: &str, message_id: &str) -> Result<String> {
    let rows = store::get_message(pool, chat_id, message_id).await?;
    Ok(render::render_history(
        &rows,
        &format!("message {message_id}"),
    ))
}

pub async fn revisions(pool: &PgPool, chat_id: &str, message_id: &str) -> Result<String> {
    // The message is read first, where the Python reads the revisions first and throws
    // them away when there is no message. Same output, one query instead of two on the
    // path a mistyped id takes.
    let message = store::get_message(pool, chat_id, message_id).await?;
    let Some(message) = message.first() else {
        return Ok(format!("no message {message_id} in chat {chat_id}"));
    };
    let rows = store::list_revisions(pool, chat_id, message_id).await?;
    Ok(render::render_revisions(
        &rows,
        &format!("{} revision(s) of {message_id}, oldest first", rows.len()),
        message.deleted_at,
    ))
}

pub async fn search(
    pool: &PgPool,
    query: &str,
    person: Option<&str>,
    chat_id: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let rows = store::search_messages(
        pool,
        query,
        person,
        chat_id,
        utc(parse_moment(before)?),
        utc(parse_moment(after)?),
        clamp(limit, 25),
    )
    .await?;
    Ok(render::render_search(
        &rows,
        &format!("{} match(es) for {}", rows.len(), py_repr(query)),
    ))
}

pub async fn people(pool: &PgPool, name: &str) -> Result<String> {
    Ok(render::render_people(
        &store::find_people(pool, name, 20).await?,
    ))
}

pub async fn status(pool: &PgPool) -> Result<String> {
    Ok(render::render_status(&store::sync_status(pool).await?))
}

/// The one tool that writes, and it writes only the two alias tables.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    participant_ids: &[String],
    note: Option<&str>,
) -> Result<String> {
    if participant_ids.is_empty() {
        let removed = store::delete_alias(pool, alias).await?;
        let outcome = if removed { "removed" } else { "did not exist" };
        return Ok(format!("alias {} {outcome}", py_repr(alias)));
    }
    store::set_alias(pool, alias, participant_ids, note).await?;
    Ok(format!(
        "alias {} now covers {} participant(s)",
        py_repr(alias),
        participant_ids.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp(None, 50), 50);
        assert_eq!(clamp(Some(0), 50), 50);
        assert_eq!(clamp(Some(-3), 25), 25);
        assert_eq!(clamp(Some(10), 50), 10);
        assert_eq!(clamp(Some(10_000), 50), MAX_LIMIT);
    }

    #[test]
    fn an_iso_timestamp_keeps_the_offset_it_was_written_with() {
        // `chats` echoes the parsed moment in its header, and `fromisoformat` keeps the
        // offset rather than converting to UTC.
        let parsed = parse_moment(Some("2026-08-14T10:00:00+02:00"))
            .unwrap()
            .unwrap();
        assert_eq!(iso_minutes(parsed), "2026-08-14T10:00+02:00");
        assert_eq!(
            parsed.with_timezone(&Utc).to_rfc3339(),
            "2026-08-14T08:00:00+00:00"
        );
    }

    #[test]
    fn a_zulu_timestamp_is_read_as_utc() {
        let parsed = parse_moment(Some("2026-08-14T10:00:00Z")).unwrap().unwrap();
        assert_eq!(iso_minutes(parsed), "2026-08-14T10:00+00:00");
    }

    #[test]
    fn a_naive_timestamp_is_utc_not_the_servers_timezone() {
        // `_moment` ends in `parsed.replace(tzinfo=UTC)`, so a value with no offset is
        // UTC even though the container sets TZ=Europe/Warsaw.
        let parsed = parse_moment(Some("2026-08-14T10:00")).unwrap().unwrap();
        assert_eq!(iso_minutes(parsed), "2026-08-14T10:00+00:00");
    }

    #[test]
    fn a_plain_date_and_a_space_separator_are_both_fine() {
        assert_eq!(
            iso_minutes(parse_moment(Some("2026-07-22")).unwrap().unwrap()),
            "2026-07-22T00:00+00:00"
        );
        assert_eq!(
            iso_minutes(parse_moment(Some("2026-07-22 08:30:00")).unwrap().unwrap()),
            "2026-07-22T08:30+00:00"
        );
    }

    #[test]
    fn an_epoch_is_accepted_as_a_time() {
        // WAHA's own payloads carry seconds since the epoch, so what this server prints
        // is not the only thing it has to read.
        let parsed = parse_moment(Some("1723640400")).unwrap().unwrap();
        assert_eq!(parsed.timestamp(), 1_723_640_400);
    }

    #[test]
    fn an_unreadable_value_is_a_usage_error_that_names_it() {
        let error = parse_moment(Some("last tuesday")).unwrap_err();
        assert!(
            matches!(&error, ToolError::Usage(message)
                     if message == "could not read 'last tuesday' as a time"),
            "{error}"
        );
    }

    #[test]
    fn no_value_is_no_filter_rather_than_an_error() {
        assert!(parse_moment(None).unwrap().is_none());
        assert!(parse_moment(Some("")).unwrap().is_none());
    }

    #[test]
    fn a_header_quotes_a_query_the_way_python_does() {
        assert_eq!(py_repr("pierogi"), "'pierogi'");
        assert_eq!(py_repr("don't"), "\"don't\"");
        assert_eq!(py_repr("it's \"fine\""), "'it\\'s \"fine\"'");
        assert_eq!(py_repr("back\\slash"), "'back\\\\slash'");
    }
}

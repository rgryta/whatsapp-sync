//! Every SQL statement in the project.
//!
//! No other module writes a query. Functions are plain `async fn` over a `PgPool`
//! rather than methods on a type, so tests can call them directly without constructing
//! a service.
//!
//! Two rules hold throughout. Queries are **runtime-checked** `sqlx::query*`, never the
//! `query_as!` macros: those need a live `DATABASE_URL` at build time or a committed
//! `.sqlx/` cache regenerated on every SQL edit, inside what has to stay a hermetic
//! Docker build — and `tests/archive/store.rs` already runs every query against a real
//! Postgres. And **no caller value is ever interpolated into SQL**: the fragments below
//! are `concat!`-composed `&'static str`, which is also what sqlx's `SqlSafeStr` bound
//! enforces, so a query built from a `String` does not type-check. Where a direction or
//! a filter has to vary there are two static constants rather than one formatted query.
//!
//! The read half, the two alias writers (the MCP role's only write authority:
//! `0001_init.sql` grants it nothing else), and in [`writes`] the ingester's writers for
//! the Rust client.

// The shared SQL fragments, as macros rather than `const`s so the queries below can
// stay `&'static str` through `concat!`. They are the same text as the Python
// module-level constants of the same name.

/// Name resolution, in one place. Order: alias, address-book name, pushName, phone.
///
/// Measured on the live account 2026-08-15: 36 of 69 DM chats resolve to a contact
/// name, 24 more to a pushName, 9 to neither. The alias comes first because the name a
/// person is actually called ("Hetty") is frequently in none of WhatsApp's fields.
macro_rules! display_name {
    () => {
        "
  COALESCE(
    (SELECT am.alias FROM alias_member am
      WHERE am.participant_id = p.participant_id LIMIT 1),
    p.contact_name,
    p.push_name,
    p.phone,
    p.participant_id
  )
"
    };
}

/// One human can hold more than one participant row — a `@c.us` contact and a
/// `@s.whatsapp.net` or lid form seen elsewhere — and an unordered `LIMIT 1` picks
/// arbitrarily, so a named person renders as a bare jid whenever the nameless row
/// happens to come first.
macro_rules! prefer_named {
    () => {
        "ORDER BY (p.contact_name IS NULL), (p.push_name IS NULL), p.participant_id"
    };
}

/// A DM's counterpart may be keyed by phone jid while the chat is keyed by either that
/// or a lid, so all three are tried. Groups carry their own subject and short-circuit.
macro_rules! chat_name {
    () => {
        concat!(
            "
  COALESCE(
    c.name,
    (SELECT ",
            display_name!(),
            " FROM participant p
      WHERE p.participant_id = c.chat_id
         OR p.lid = c.chat_id
         OR split_part(p.participant_id, '@', 1) = split_part(c.chat_id, '@', 1)
      ",
            prefer_named!(),
            " LIMIT 1),
    c.chat_id
  )
"
        )
    };
}

macro_rules! author {
    () => {
        concat!(
            "
  COALESCE(
    (SELECT ",
            display_name!(),
            " FROM participant p
      WHERE p.participant_id = m.author_id
         OR p.lid = m.author_id
         OR split_part(p.participant_id, '@', 1) = split_part(m.author_id, '@', 1)
      ",
            prefer_named!(),
            " LIMIT 1),
    m.author_id
  )
"
        )
    };
}

macro_rules! message_columns {
    () => {
        concat!(
            "
  m.chat_id, m.message_id, m.ts, m.from_me, m.kind, m.body, m.transcript,
  m.media_mime, m.media_filename, m.media_seconds, m.reply_to_id, m.ack,
  m.deleted_at,
  -- Derived, never stored: \"was this edited\" is exactly \"is there a history of it\",
  -- and a column saying so could drift from the rows it describes.
  EXISTS (SELECT 1 FROM message_revision v
           WHERE v.chat_id = m.chat_id AND v.message_id = m.message_id) AS edited,
  ",
            author!(),
            " AS author,
  -- Who reacted, not just what. 513 of 1,252 reacted messages carry more than one
  -- reaction, so a bare emoji list silently merges several people into one mark.
  (SELECT string_agg(
     r.emoji || ' ' || COALESCE(
       (SELECT ",
            display_name!(),
            " FROM participant p
         WHERE p.participant_id = r.actor_id
            OR p.lid = r.actor_id
            OR split_part(p.participant_id, '@', 1) = split_part(r.actor_id, '@', 1)
         ",
            prefer_named!(),
            " LIMIT 1),
       r.actor_id),
     ', ' ORDER BY r.ts NULLS LAST)
   FROM reaction r
   WHERE r.chat_id = m.chat_id AND r.message_id = m.message_id) AS reactions
"
        )
    };
}

/// The accent fold, one-to-one and lowercase: Polish first, then the other Latin
/// diacritics. A macro so the SQL below can `concat!` it; [`FOLD_FROM`] and [`FOLD_TO`]
/// are the same text for [`fold`], so the two sides cannot drift apart.
macro_rules! fold_from {
    () => {
        "ąćęłńóśźżáàâäãåéèêëěíìîïòôöõőúùûüůűýÿñňçčšžřďť"
    };
}
macro_rules! fold_to {
    () => {
        "acelnoszzaaaaaaeeeeeiiiiooooouuuuuuyynnccszrdt"
    };
}

pub(crate) const FOLD_FROM: &str = fold_from!();
pub(crate) const FOLD_TO: &str = fold_to!();

/// A column folded the way [`fold`] folds the caller's input: `lower()` first, so an
/// uppercase letter (Ł included) reaches the table as the lowercase one it lists.
macro_rules! folded {
    ($col:literal) => {
        concat!(
            "translate(lower(",
            $col,
            "), '",
            fold_from!(),
            "', '",
            fold_to!(),
            "')"
        )
    };
}

/// The caller's side of [`folded!`]: lowercase, then the same one-to-one mapping, so
/// "Michal", "michał" and "MICHAŁ" all compare equal to a stored "Michał".
pub(crate) fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| {
            FOLD_FROM
                .chars()
                .zip(FOLD_TO.chars())
                .find(|&(from, _)| from == c)
                .map_or(c, |(_, to)| to)
        })
        .collect()
}

/// Matches a person by any name form they are known under, ignoring case and accents.
///
/// The parameter must already be [`fold`]ed. The parameter number is a literal because
/// the fragment is used at `$1` in `find_people` and at `$3` in `search_messages`; the
/// Python does the same thing with `.replace("$PARAM", "$1")`. Folding the column
/// costs any name index, which is fine: the people tables are small.
macro_rules! person_match {
    ($param:literal) => {
        concat!(
            "
  (",
            folded!("p.contact_name"),
            " LIKE '%' || ",
            $param,
            " || '%'
    OR ",
            folded!("p.push_name"),
            " LIKE '%' || ",
            $param,
            " || '%'
    OR p.phone     ILIKE '%' || ",
            $param,
            " || '%'
    OR EXISTS (SELECT 1 FROM alias_member am
                WHERE am.participant_id = p.participant_id
                  AND ",
            folded!("am.alias"),
            " LIKE '%' || ",
            $param,
            " || '%'))
"
        )
    };
}

mod aliases;
mod chats;
mod messages;
mod people;
mod status;
pub mod writes;

pub use aliases::{delete_alias, set_alias};
pub use chats::list_chats;
pub use messages::{count_history, get_history, get_message, list_revisions, search_messages};
pub use people::find_people;
pub use status::sync_status;

#[cfg(test)]
mod tests {
    use super::{FOLD_FROM, FOLD_TO, fold};

    #[test]
    fn the_rust_fold_and_the_sql_table_agree() {
        assert_eq!(FOLD_FROM.chars().count(), FOLD_TO.chars().count());
        assert!(FOLD_TO.chars().all(|c| c.is_ascii_lowercase()));
        let sql = folded!("x");
        assert!(sql.contains(FOLD_FROM) && sql.contains(FOLD_TO), "{sql}");
        for (from, to) in FOLD_FROM.chars().zip(FOLD_TO.chars()) {
            assert_eq!(FOLD_FROM.matches(from).count(), 1, "{from} listed twice");
            assert_eq!(fold(&from.to_string()), to.to_string(), "{from}");
            assert_eq!(
                fold(&from.to_uppercase().to_string()),
                to.to_string(),
                "{from}"
            );
        }
        assert_eq!(fold("ŁUKASZ Michał"), "lukasz michal");
    }
}

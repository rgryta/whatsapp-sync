//! `participant`: resolving a name fragment to people.

use sqlx::postgres::PgPool;

use crate::models::Person;

pub async fn find_people(
    pool: &PgPool,
    fragment: &str,
    limit: i64,
) -> Result<Vec<Person>, sqlx::Error> {
    sqlx::query_as::<_, Person>(concat!(
        "
        SELECT p.participant_id, p.lid, p.phone, p.contact_name, p.push_name,
               ",
        display_name!(),
        " AS display_name,
               (SELECT string_agg(am.alias, ', ') FROM alias_member am
                 WHERE am.participant_id = p.participant_id) AS aliases,
               -- Participants are keyed @c.us, DM chats @s.whatsapp.net, so a
               -- participant_id is never a usable chat_id. Hand back the id that is.
               (SELECT c.chat_id FROM chat c
                 WHERE c.kind = 'dm'
                   AND split_part(c.chat_id, '@', 1)
                       = COALESCE(p.phone, split_part(p.participant_id, '@', 1))
                 LIMIT 1) AS chat_id
        FROM participant p
        WHERE ",
        person_match!("$1"),
        "
        ORDER BY display_name
        LIMIT $2
        "
    ))
    .bind(fragment)
    .bind(limit)
    .fetch_all(pool)
    .await
}

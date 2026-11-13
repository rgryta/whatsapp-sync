-- Repair `author_id` on messages stored before the sender was read correctly.
--
-- WAHA puts the sender in three places. Early ingestion only checked the top-level
-- `participant`/`author` and then fell through to `from`, which in a group is the
-- group itself — so 90% of group messages were archived with a chat id in the author
-- column, rendering as a chat pretending to be a person.
--
-- `raw` is stored verbatim, so the correct value is already here and this needs no
-- re-fetch. Runs before any sweep on a fresh database, where it matches nothing.
UPDATE message m
SET author_id = COALESCE(
      m.raw #>> '{participant}',
      m.raw #>> '{author}',
      m.raw #>> '{_data,participant}',
      m.raw #>> '{_data,key,participant}',
      m.raw #>> '{_data,key,participantAlt}'
    )
FROM chat c
WHERE c.chat_id = m.chat_id
  AND c.kind = 'group'
  AND m.author_id = m.chat_id;

-- A group message with no sender anywhere in its payload is genuinely unattributable;
-- NULL says so, where the chat id claimed otherwise.
UPDATE message m
SET author_id = NULL
FROM chat c
WHERE c.chat_id = m.chat_id
  AND c.kind = 'group'
  AND m.author_id = m.chat_id;

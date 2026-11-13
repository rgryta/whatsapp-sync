-- whatsapp-sync initial schema.
--
-- WhatsApp timestamps are whole seconds, so a burst of messages ties on `ts`. Every
-- ordering in this project is (ts, message_id): deterministic and stable, though
-- within one second it is an order, not necessarily the order.

CREATE TABLE participant (
  participant_id text PRIMARY KEY,          -- phone jid, e.g. 447700900123@c.us
  lid            text UNIQUE,               -- opaque id carried on group messages
  phone          text,
  contact_name   text,                      -- from the address book
  push_name      text,                      -- self-reported, mutable
  first_seen     timestamptz NOT NULL DEFAULT now(),
  last_seen      timestamptz NOT NULL DEFAULT now()
);

-- `watermark_ts` is how far this chat has been swept. It is written only after a
-- page's messages are committed, never by chat discovery, which knows nothing about
-- crawl progress and would rewind it on every pass.
CREATE TABLE chat (
  chat_id        text PRIMARY KEY,
  kind           text NOT NULL CHECK (kind IN ('dm','group')),
  name           text,
  watermark_ts   timestamptz,
  last_synced_at timestamptz,
  raw            jsonb
);

CREATE TABLE message (
  chat_id          text NOT NULL REFERENCES chat(chat_id),
  message_id       text NOT NULL,
  ts               timestamptz NOT NULL,
  from_me          boolean NOT NULL DEFAULT false,
  author_id        text,
  body             text,
  kind             text NOT NULL DEFAULT 'text',
  reply_to_id      text,
  ack              integer,
  has_media        boolean NOT NULL DEFAULT false,
  media_mime       text,
  media_filename   text,
  media_seconds    integer,
  transcript       text,
  transcript_state text NOT NULL DEFAULT 'n/a'
                   CHECK (transcript_state IN ('pending','done','failed','n/a')),
  -- Stored verbatim. The media envelope it carries (url, mediaKey, hashes, and a
  -- jpegThumbnail) is the only handle a future media fetcher would have, and
  -- stripping it saves under 2MB across the whole corpus.
  raw              jsonb NOT NULL,
  -- 'simple', not a language configuration: the corpus mixes Polish and English and
  -- no single stemmer serves both. Transcripts are indexed with bodies so a voice
  -- note is findable by what was said in it.
  search           tsvector GENERATED ALWAYS AS (
                     to_tsvector('simple',
                       coalesce(body,'') || ' ' || coalesce(transcript,''))
                   ) STORED,
  PRIMARY KEY (chat_id, message_id)
);

CREATE INDEX message_chat_order_idx ON message (chat_id, ts, message_id);
CREATE INDEX message_ts_idx         ON message (ts);
CREATE INDEX message_author_idx     ON message (author_id);
CREATE INDEX message_search_idx     ON message USING GIN (search);
-- Partial: the transcription worker asks only for pending rows, and they are a
-- fraction of a percent of the table.
CREATE INDEX message_pending_transcript_idx
  ON message (chat_id, message_id) WHERE transcript_state = 'pending';

-- Reactions are a snapshot, not a log: what WAHA reports for a message is the whole
-- truth about it, so the stored set is replaced rather than appended to.
CREATE TABLE reaction (
  chat_id    text NOT NULL,
  message_id text NOT NULL,
  actor_id   text NOT NULL,
  emoji      text NOT NULL,
  ts         timestamptz,
  PRIMARY KEY (chat_id, message_id, actor_id),
  FOREIGN KEY (chat_id, message_id) REFERENCES message(chat_id, message_id) ON DELETE CASCADE
);

-- The name a person actually goes by is frequently in none of WhatsApp's fields.
CREATE TABLE user_alias (
  alias      text PRIMARY KEY,
  note       text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE alias_member (
  alias          text NOT NULL REFERENCES user_alias(alias) ON DELETE CASCADE,
  participant_id text NOT NULL,
  PRIMARY KEY (alias, participant_id)
);

CREATE TABLE sync_state (
  loop          text PRIMARY KEY CHECK (loop IN ('sync','transcribe')),
  last_run_at   timestamptz,
  last_error    text,
  last_error_at timestamptz
);

-- The MCP role reads everything and writes only aliases. Guarded because the role
-- does not exist on a throwaway test container, and a hard GRANT would fail there.
DO $$ BEGIN
  EXECUTE 'GRANT SELECT ON ALL TABLES IN SCHEMA public TO whatsapp_mcp';
  EXECUTE 'GRANT INSERT, UPDATE, DELETE ON user_alias, alias_member TO whatsapp_mcp';
EXCEPTION WHEN undefined_object THEN
  RAISE NOTICE 'role whatsapp_mcp absent; skipping grants';
END $$;

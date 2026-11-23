-- Keep what a message used to say, and mark the ones that went away.
--
-- WAHA hands us no edit event: `editedMessage` appears in 0 of 8152 stored payloads.
-- The only way to learn that a message changed is to read it again and compare, so
-- these tables record what a re-read found rather than what a notification told us.
--
-- No backfill. Every message already stored reads as "never edited, not deleted",
-- which is exactly what this archive knows about it.

CREATE TABLE message_revision (
  chat_id       text        NOT NULL,
  message_id    text        NOT NULL,
  seq           int         NOT NULL,
  -- Nullable because a message can be edited from having text to having none.
  text          text,
  -- When this text became the current one, as far as this archive can tell.
  --
  -- NULL on seq 1 and only there, and it means "unknown", not "missing": nothing ever
  -- recorded when a message was first stored, so the earliest text we hold has no
  -- honest start time. Every later revision has an exact one -- the sweep that found
  -- the change. Writing the message's own `ts` here instead would look tidier and
  -- claim something different (when it was sent, not when we first saw this text).
  first_seen_at timestamptz,
  -- NULL marks the revision that is current. Exactly one row per message has it.
  superseded_at timestamptz,
  PRIMARY KEY (chat_id, message_id, seq),
  FOREIGN KEY (chat_id, message_id) REFERENCES message (chat_id, message_id) ON DELETE CASCADE
);

-- Every read of this table is "the history of one message, in order".
CREATE INDEX message_revision_seq_idx ON message_revision (chat_id, message_id, seq);

-- At most one current revision per message. The invariant the readers rely on is
-- enforced here rather than trusted to the writer, because a revision log that can
-- disagree with the message it describes is worse than no revision log.
CREATE UNIQUE INDEX message_revision_one_current_idx
  ON message_revision (chat_id, message_id)
  WHERE superseded_at IS NULL;

-- deleted_at: the source stopped returning a message we hold, twice running.
-- missing_since: it stopped returning it once. An absence is not evidence on its own
-- -- a short page, a dropped connection and an expired filter all look identical to
-- a deletion -- so nothing is claimed until a second clean sweep agrees.
ALTER TABLE message ADD COLUMN deleted_at    timestamptz;
ALTER TABLE message ADD COLUMN missing_since timestamptz;

-- The sweep asks "which messages do I hold in this window", every cycle, per chat.
CREATE INDEX message_sweep_idx ON message (chat_id, ts) WHERE deleted_at IS NULL;

DO $$
DECLARE
  has_archive boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'archive_data');
  has_nvme    boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes');
BEGIN
  IF NOT (has_archive AND has_nvme) THEN
    RAISE NOTICE 'archive_data/nvme_indexes absent; leaving the new objects on the default tablespace';
    RETURN;
  END IF;
  SET LOCAL lock_timeout = '5s';
  EXECUTE 'ALTER TABLE message_revision SET TABLESPACE archive_data';
  EXECUTE 'ALTER INDEX message_revision_seq_idx SET TABLESPACE nvme_indexes';
  EXECUTE 'ALTER INDEX message_revision_one_current_idx SET TABLESPACE nvme_indexes';
  EXECUTE 'ALTER INDEX message_sweep_idx SET TABLESPACE nvme_indexes';
END $$;

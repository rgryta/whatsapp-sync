-- Provision the `whatsapp` database and its two roles on homelab-postgres.
--
-- Run as the postgres superuser:
--   psql -h homelab-postgres -U postgres \
--     -v ingest_pw="<ingest password>" -v mcp_pw="<mcp password>" \
--     -f deploy/provision-whatsapp-db.sql
--
-- NOTE: pass RAW passwords. :'ingest_pw' quotes the value once; pre-quoting stores
-- the literal quotes in the role password, and authentication then fails with a
-- message that says nothing about quoting.

CREATE ROLE whatsapp_ingest LOGIN PASSWORD :'ingest_pw';
CREATE ROLE whatsapp_mcp    LOGIN PASSWORD :'mcp_pw';

CREATE DATABASE whatsapp OWNER whatsapp_ingest;

\connect whatsapp

GRANT ALL ON SCHEMA public TO whatsapp_ingest;
GRANT USAGE ON SCHEMA public TO whatsapp_mcp;

-- Without CREATE on the tablespaces, 0002 fails soft and the corpus silently stays on
-- the default volume. Guarded because a host without them is a valid host.
DO $$ BEGIN
  EXECUTE 'GRANT CREATE ON TABLESPACE archive_data, nvme_indexes TO whatsapp_ingest';
EXCEPTION WHEN undefined_object THEN
  RAISE NOTICE 'archive_data/nvme_indexes absent; the corpus will live on the default tablespace';
END $$;

-- The MCP role reads everything and writes only the alias tables. The table-level
-- grants live in 0001_init.sql because none of the tables exist yet at this point;
-- this default covers anything the ingester creates later.
ALTER DEFAULT PRIVILEGES FOR ROLE whatsapp_ingest IN SCHEMA public
  GRANT SELECT ON TABLES TO whatsapp_mcp;

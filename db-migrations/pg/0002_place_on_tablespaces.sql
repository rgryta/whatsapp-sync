-- Place tables on archive_data and indexes on nvme_indexes, where those exist.
--
-- Guarded rather than declared with TABLESPACE clauses on CREATE: a throwaway test
-- container has neither tablespace, and a hard clause would fail the whole schema
-- there. Skipping rows whose reltablespace is already set makes a re-run cheap.
DO $$
DECLARE
  r record;
  has_archive boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'archive_data');
  has_nvme    boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes');
BEGIN
  IF NOT (has_archive AND has_nvme) THEN
    RAISE NOTICE 'archive_data/nvme_indexes absent; leaving everything on the default tablespace';
    RETURN;
  END IF;
  -- Bounded: an ALTER ... SET TABLESPACE takes an ACCESS EXCLUSIVE lock, and waiting
  -- forever behind a running sweep is worse than failing the migration.
  SET LOCAL lock_timeout = '5s';
  FOR r IN
    SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public' AND c.relkind = 'r' AND c.reltablespace = 0
  LOOP
    EXECUTE format('ALTER TABLE public.%I SET TABLESPACE archive_data', r.relname);
  END LOOP;
  FOR r IN
    SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public' AND c.relkind = 'i' AND c.reltablespace = 0
  LOOP
    EXECUTE format('ALTER INDEX public.%I SET TABLESPACE nvme_indexes', r.relname);
  END LOOP;
END $$;

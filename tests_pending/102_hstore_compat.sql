-- hstore compatibility shim (SQLAlchemy/psycopg2 connect-time probe)

-- Should succeed (metadata-only).
CREATE EXTENSION IF NOT EXISTS hstore;

-- psycopg2 probes pg_type/pg_namespace to discover hstore OIDs.
SELECT
  COUNT(*) AS hstore_rows,
  COALESCE(MIN(t.typarray), 0) > 0 AS has_typarray
FROM pg_type t
JOIN pg_namespace ns ON ns.oid = t.typnamespace
WHERE t.typname = 'hstore' AND ns.nspname = 'public';

-- Keep the suite isolated: later tests may assert `pg_extension` contents.
DROP EXTENSION IF EXISTS hstore;

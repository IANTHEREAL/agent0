-- hstore compatibility shim (SQLAlchemy/psycopg2 connect-time probe)

-- Keep case deterministic regardless of prior test state.
DROP EXTENSION IF EXISTS hstore;

-- Without extension install, PG returns NULL for hstore regtype lookups.
SELECT
  to_regtype('hstore') IS NULL AS hstore_null_before,
  to_regtype('_hstore') IS NULL AS hstore_arr_alias_null_before,
  to_regtype('hstore[]') IS NULL AS hstore_arr_type_null_before;

-- Should succeed (metadata-only).
CREATE EXTENSION IF NOT EXISTS hstore;

-- psycopg2 probes pg_type/pg_namespace to discover hstore OIDs.
SELECT
  COUNT(*) AS hstore_rows,
  COALESCE(MIN(t.typarray), 0) > 0 AS has_typarray
FROM pg_type t
JOIN pg_namespace ns ON ns.oid = t.typnamespace
WHERE t.typname = 'hstore' AND ns.nspname = 'public';

-- Once installed, hstore regtype lookups resolve.
SELECT
  to_regtype('hstore') IS NOT NULL AS hstore_visible_after_create,
  to_regtype('_hstore') IS NOT NULL AS hstore_arr_alias_visible_after_create,
  to_regtype('hstore[]') IS NOT NULL AS hstore_arr_type_visible_after_create;

-- Quoted identifiers are case-sensitive in PostgreSQL.
SELECT
  to_regtype('"HSTORE"') IS NULL AS quoted_hstore_upper_null,
  to_regtype('"Public".hstore') IS NULL AS quoted_public_schema_case_null,
  to_regtype('public."HSTORE"') IS NULL AS quoted_hstore_type_case_null;

-- Keep the suite isolated: later tests may assert `pg_extension` contents.
DROP EXTENSION IF EXISTS hstore;

-- Dropped again -> NULL.
SELECT
  to_regtype('hstore') IS NULL AS hstore_null_after_drop,
  to_regtype('_hstore') IS NULL AS hstore_arr_alias_null_after_drop,
  to_regtype('hstore[]') IS NULL AS hstore_arr_type_null_after_drop;

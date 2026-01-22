-- PostgreSQL compatible tests from redact_descriptor
-- 3 tests

-- Test 1: statement (line 34)
CREATE TABLE foo (
    i BIGINT PRIMARY KEY DEFAULT 42,
    j BIGINT GENERATED ALWAYS AS (44) STORED
);
CREATE INDEX foo_j_idx ON foo (j) WHERE (i = 41);

-- CockroachDB has a redacted descriptor facility; model a stable descriptor for
-- PostgreSQL using catalog/introspection views (no OIDs/IDs).
CREATE OR REPLACE VIEW redacted_descriptors AS
WITH cols AS (
  SELECT
    format('%I.%I', table_schema, table_name)::regclass AS id,
    jsonb_agg(
      jsonb_build_object(
        'column_name', column_name,
        'data_type', data_type,
        'column_default', column_default,
        'is_generated', is_generated,
        'generation_expression', generation_expression
      )
      ORDER BY ordinal_position
    ) AS columns
  FROM information_schema.columns
  WHERE table_schema = 'public'
  GROUP BY 1
),
idx AS (
  SELECT
    t.oid::regclass AS id,
    jsonb_agg(
      jsonb_build_object(
        'index_name', ic.relname,
        'indexdef', pg_get_indexdef(i.indexrelid)
      )
      ORDER BY ic.relname
    ) AS indexes
  FROM pg_index i
  JOIN pg_class t ON t.oid = i.indrelid
  JOIN pg_class ic ON ic.oid = i.indexrelid
  JOIN pg_namespace n ON n.oid = t.relnamespace
  WHERE n.nspname = 'public'
  GROUP BY 1
)
SELECT
  cols.id,
  jsonb_build_object(
    'id', cols.id::text,
    'columns', cols.columns,
    'indexes', COALESCE(idx.indexes, '[]'::jsonb)
  )::text AS descriptor
FROM cols
LEFT JOIN idx ON idx.id = cols.id;

-- onlyif config schema-locked-disabled

-- Test 2: query (line 43)
SELECT descriptor FROM redacted_descriptors WHERE id = 'foo'::REGCLASS;

-- Test 3: query (line 179)
SELECT descriptor FROM redacted_descriptors WHERE id = 'foo'::REGCLASS;

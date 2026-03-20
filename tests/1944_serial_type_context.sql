-- 1944: Verify unified resolve_custom_type pipeline
-- Serial pseudo-types expand ONLY in DDL column context; 42704 elsewhere.
-- Built-in custom types (jsonb, tsvector, regclass, etc.) resolve everywhere.

-- ── Setup ──────────────────────────────────────────────────────────────────

DROP TABLE IF EXISTS serial_ctx_t CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

-- ── 1. Serial in DDL column context → expand to int + sequence ─────────

CREATE TABLE serial_ctx_t (
    id serial PRIMARY KEY,
    big_id bigserial
);

-- Verify column types resolved correctly
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_name = 'serial_ctx_t' AND table_schema = 'public'
ORDER BY ordinal_position;

-- Verify sequences were created
INSERT INTO serial_ctx_t DEFAULT VALUES;
SELECT id, big_id FROM serial_ctx_t;

-- ── 2. Serial in non-DDL contexts → 42704 ─────────────────────────────

-- CAST
SELECT NULL::serial;

-- PREPARE parameter type
PREPARE serial_param(serial) AS SELECT $1;

-- CREATE TYPE composite field
CREATE TYPE bad_composite AS (f serial);

-- ── 3. Built-in custom type resolution ────────────────────────────────

SELECT NULL::jsonb IS NULL AS jsonb_ok;
SELECT NULL::tsvector IS NULL AS tsvector_ok;
SELECT NULL::timestamptz IS NULL AS timestamptz_ok;
SELECT NULL::name IS NULL AS name_ok;

-- ── 4. Enum UDT resolution via catalog ────────────────────────────────

CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral');
CREATE TABLE mood_tbl (m mood DEFAULT 'happy'::mood);
INSERT INTO mood_tbl DEFAULT VALUES;
INSERT INTO mood_tbl (m) VALUES ('sad');
SELECT m FROM mood_tbl ORDER BY m;

-- ── Cleanup ───────────────────────────────────────────────────────────

DROP TABLE IF EXISTS serial_ctx_t CASCADE;
DROP TABLE IF EXISTS mood_tbl CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

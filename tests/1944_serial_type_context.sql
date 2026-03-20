-- Issue #1944 / #1949: serial pseudo-types must resolve only in DDL column
-- context. All other DataType::Custom paths must use catalog lookup first,
-- then built-in mapping, and fail with 42704 on unknown types.

DROP TABLE IF EXISTS serial_ctx_seq CASCADE;
DROP TABLE IF EXISTS serial_ctx_pseudo_wins CASCADE;
DROP TABLE IF EXISTS mood_alt_tbl CASCADE;
DROP TABLE IF EXISTS mood_tbl CASCADE;
DROP TYPE IF EXISTS bad_composite CASCADE;
DROP TYPE IF EXISTS good_composite CASCADE;
DROP TYPE IF EXISTS serial CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

CREATE TABLE serial_ctx_seq (
    id serial PRIMARY KEY,
    big_id bigserial
);

SELECT 'ddl_serial_columns' AS check_name,
       string_agg(column_name || ':' || data_type, ',' ORDER BY ordinal_position) AS details
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 'serial_ctx_seq';

INSERT INTO serial_ctx_seq DEFAULT VALUES;
SELECT 'ddl_serial_insert' AS check_name, id, big_id
FROM serial_ctx_seq
ORDER BY id;

ALTER TABLE serial_ctx_seq ALTER COLUMN id TYPE serial;
SELECT NULL::serial;
PREPARE serial_param(serial) AS SELECT $1;
CREATE TYPE bad_composite AS (f serial);
SELECT NULL::serial[];

SELECT 'builtin_jsonb' AS check_name, pg_typeof(NULL::jsonb)::text AS type_name;
SELECT 'builtin_tsvector' AS check_name, pg_typeof(NULL::tsvector)::text AS type_name;
SELECT 'builtin_timestamptz' AS check_name, pg_typeof(NULL::timestamptz)::text AS type_name;
SELECT 'builtin_name' AS check_name, pg_typeof(NULL::name)::text AS type_name;
SELECT 'builtin_regclass' AS check_name, pg_typeof(NULL::regclass)::text AS type_name;
SELECT 'builtin_regtype' AS check_name, pg_typeof(NULL::regtype)::text AS type_name;

CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral');
CREATE TABLE mood_tbl (m mood DEFAULT 'happy'::mood);
CREATE TABLE mood_alt_tbl (m mood);
INSERT INTO mood_tbl DEFAULT VALUES;
INSERT INTO mood_tbl (m) VALUES ('sad');
INSERT INTO mood_alt_tbl (m) VALUES ('happy'), ('sad');
SELECT 'enum_default_insert' AS check_name,
       string_agg(m::text, ',' ORDER BY m::text) AS values_seen
FROM mood_tbl;

CREATE TYPE serial AS ENUM ('happy', 'sad', 'neutral');

SELECT 'serial_udt_cast' AS check_name, NULL::serial IS NULL AS ok;
PREPARE serial_param_ok(serial) AS SELECT $1;
SELECT 'serial_udt_prepare' AS check_name, 'ok' AS status;
DEALLOCATE serial_param_ok;
CREATE TYPE good_composite AS (f serial);
SELECT 'serial_udt_composite' AS check_name, 'ok' AS status;
SELECT 'serial_udt_array' AS check_name, NULL::serial[] IS NULL AS ok;

ALTER TABLE mood_alt_tbl ALTER COLUMN m TYPE serial USING m::text::serial;
SELECT 'serial_udt_alter' AS check_name, udt_name
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 'mood_alt_tbl' AND column_name = 'm';

CREATE TABLE serial_ctx_pseudo_wins (c serial);
SELECT 'serial_pseudo_wins' AS check_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 'serial_ctx_pseudo_wins' AND column_name = 'c';

DROP TABLE IF EXISTS serial_ctx_seq CASCADE;
DROP TABLE IF EXISTS serial_ctx_pseudo_wins CASCADE;
DROP TABLE IF EXISTS mood_alt_tbl CASCADE;
DROP TABLE IF EXISTS mood_tbl CASCADE;
DROP TYPE IF EXISTS bad_composite CASCADE;
DROP TYPE IF EXISTS good_composite CASCADE;
DROP TYPE IF EXISTS serial CASCADE;
DROP TYPE IF EXISTS mood CASCADE;

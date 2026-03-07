-- Sequence relation read support for state export (#1549)

DROP TABLE IF EXISTS seqrel_serial_t;
DROP SEQUENCE IF EXISTS seqrel_standalone;
DROP SCHEMA IF EXISTS seqrel_schema CASCADE;
DROP SEQUENCE IF EXISTS "MySeq";

-- 1) Standalone sequence initial state.
CREATE SEQUENCE seqrel_standalone START WITH 1;
SELECT 's1_init=' || last_value::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM seqrel_standalone;

-- 2) nextval updates exported state.
SELECT nextval('seqrel_standalone');
SELECT 's2_after_nextval=' || last_value::text || ',' || log_cnt::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM seqrel_standalone;

-- 3) setval(..., false) updates exported state.
SELECT setval('seqrel_standalone', 42, false);
SELECT 's3_after_setval_false=' || last_value::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM seqrel_standalone;

-- 4) Schema-qualified sequence relation lookup.
CREATE SCHEMA seqrel_schema;
CREATE SEQUENCE seqrel_schema.my_seq START WITH 5;
SELECT 's4_qualified=' || last_value::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM seqrel_schema.my_seq;

-- 5) SERIAL-owned sequence after INSERT.
CREATE TABLE seqrel_serial_t (
    id SERIAL PRIMARY KEY,
    payload TEXT
);
INSERT INTO seqrel_serial_t (payload) VALUES ('a'), ('b');
SELECT 's5_serial_after_insert=' || last_value::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM seqrel_serial_t_id_seq;

-- 6) Nonexistent sequence relation reports relation-not-found.
SELECT last_value, is_called FROM seqrel_missing;

-- 7) Quoted identifier sequence relation.
CREATE SEQUENCE "MySeq" START WITH 9;
SELECT 's7_quoted=' || last_value::text || ',' ||
       CASE WHEN is_called THEN 'true' ELSE 'false' END AS probe
FROM "MySeq";

DROP TABLE seqrel_serial_t;
DROP SEQUENCE seqrel_standalone;
DROP SCHEMA seqrel_schema CASCADE;
DROP SEQUENCE "MySeq";

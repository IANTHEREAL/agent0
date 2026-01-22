-- PostgreSQL compatible tests from zone_config
-- 75 tests

-- NOTE: CockroachDB zone configs (`ALTER ... CONFIGURE ZONE`, `ALTER RANGE ...`)
-- and related internal catalogs (`crdb_internal`, `system.*`) do not exist in
-- PostgreSQL. The Cockroach-only statements are commented out so the remaining
-- portable DDL/catelog checks can run under PG.

SET client_min_messages = warning;

-- Test 1: statement (line 3)
-- ALTER RANGE default CONFIGURE ZONE USING num_replicas = 1;

-- Test 2: statement (line 19)
-- ALTER RANGE default CONFIGURE ZONE USING DEFAULT;

-- Test 3: statement (line 35)
-- ALTER RANGE default CONFIGURE ZONE USING range_min_bytes = 1234567;

-- Test 4: statement (line 38)
CREATE TABLE a (id INT PRIMARY KEY);

-- Test 5: statement (line 57)
-- ALTER TABLE a CONFIGURE ZONE USING DEFAULT;

-- Test 6: statement (line 72)
-- ALTER TABLE a CONFIGURE ZONE USING
--   range_min_bytes = 200000 + 1,
--   range_max_bytes = 100000000 + 1,
--   gc.ttlseconds = 3000 + 600,
--   num_replicas = floor(1.2)::int,
--   constraints = '[+region=test]',
--   lease_preferences = '[[+region=test]]';

-- Test 7: query (line 82)
-- SELECT feature_name FROM crdb_internal.feature_usage
-- WHERE feature_name IN (
--   'sql.schema.zone_config.table.range_min_bytes',
--   'sql.schema.zone_config.table.range_max_bytes',
--   'sql.schema.zone_config.table.gc.ttlseconds',
--   'sql.schema.zone_config.table.num_replicas',
--   'sql.schema.zone_config.table.constraints'
-- ) AND usage_count > 0 ORDER BY feature_name;

-- Test 8: statement (line 110)
-- ALTER TABLE a CONFIGURE ZONE USING range_max_bytes = 400000000;

-- Test 9: statement (line 127)
CREATE SCHEMA test;

-- Test 10: statement (line 130)
CREATE TABLE test.a (a INT PRIMARY KEY);

-- Test 11: statement (line 133)
-- ALTER TABLE test.a CONFIGURE ZONE USING gc.ttlseconds=1234;

-- Test 12: query (line 162)
SELECT column_name, data_type, is_nullable
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'a'
ORDER BY ordinal_position;

-- Test 13: query (line 178)
SELECT indexname, indexdef
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 'a'
ORDER BY indexname;

-- Test 14: statement (line 195)
-- ALTER TABLE a CONFIGURE ZONE USING DEFAULT;

-- Test 15: statement (line 213)
-- ALTER TABLE a CONFIGURE ZONE DISCARD;

-- Test 16: query (line 223)
-- SELECT feature_name FROM crdb_internal.feature_usage
-- WHERE feature_name IN ('sql.schema.alter_range.configure_zone', 'sql.schema.alter_table.configure_zone')
-- ORDER BY feature_name;

-- Test 17: statement (line 236)
-- ALTER TABLE a CONFIGURE ZONE USING voter_constraints = '{\"+region=test\": 3}';

-- Test 18: statement (line 241)
-- ALTER TABLE a CONFIGURE ZONE USING num_replicas = 3;

-- Test 19: statement (line 255)
-- ALTER TABLE a CONFIGURE ZONE USING num_voters = 1;

-- Test 20: statement (line 272)
-- ALTER TABLE a CONFIGURE ZONE USING voter_constraints = '{\"+region=test\": 1}';

-- Test 21: statement (line 275)
-- ALTER TABLE a CONFIGURE ZONE USING num_voters = COPY FROM PARENT;

-- Test 22: statement (line 293)
-- ALTER TABLE a CONFIGURE ZONE USING voter_constraints = '{\"+region=shouldFail\": 1}';

-- Test 23: statement (line 299)
CREATE TABLE zc (
  a INT PRIMARY KEY,
  b INT
);

-- Test 24: statement (line 306)
INSERT INTO zc VALUES (1,2);

-- Test 25: statement (line 309)
-- ALTER TABLE zc CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- Test 26: statement (line 312)
-- SET autocommit_before_ddl = false;

-- Test 27: statement (line 315)
CREATE MATERIALIZED VIEW vm (x, y) AS SELECT a,b FROM zc;
-- ALTER TABLE vm CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- Test 28: statement (line 318)
CREATE VIEW v (x, y) AS SELECT a, b FROM zc;
-- ALTER TABLE v CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- Test 29: statement (line 321)
-- RESET autocommit_before_ddl;

-- user root

-- onlyif config local-legacy-schema-changer

-- Test 30: statement (line 327)
-- ALTER TABLE pg_catalog.pg_type CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- onlyif config local-legacy-schema-changer

-- Test 31: statement (line 331)
-- ALTER TABLE information_schema.columns CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- skipif config local-legacy-schema-changer

-- Test 32: statement (line 335)
-- ALTER TABLE pg_catalog.pg_type CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- skipif config local-legacy-schema-changer

-- Test 33: statement (line 339)
-- ALTER TABLE information_schema.columns CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- Test 34: statement (line 342)
CREATE TABLE roachie(i int);

-- user testuser

-- Test 35: statement (line 347)
-- ALTER TABLE roachie CONFIGURE ZONE USING gc.ttlseconds = 1;

-- user root

-- Test 36: statement (line 354)
CREATE TABLE same_table_name();
-- ALTER TABLE same_table_name CONFIGURE ZONE USING gc.ttlseconds = 500;
CREATE SCHEMA alternative_schema;
CREATE TABLE alternative_schema.same_table_name();
-- ALTER TABLE alternative_schema.same_table_name CONFIGURE ZONE USING gc.ttlseconds = 600;

-- onlyif config schema-locked-disabled

-- Test 37: query (line 362)
SELECT to_regclass('same_table_name') IS NOT NULL AS exists;

-- Test 38: query (line 373)
SELECT to_regclass('same_table_name') IS NOT NULL AS exists;

-- Test 39: query (line 384)
SELECT to_regclass('alternative_schema.same_table_name') IS NOT NULL AS exists;

-- Test 40: query (line 396)
SELECT to_regclass('alternative_schema.same_table_name') IS NOT NULL AS exists;

-- Test 41: statement (line 406)
DROP TABLE zc CASCADE;

-- Test 42: statement (line 409)
-- CREATE DATABASE foo;

-- Test 43: statement (line 412)
-- ALTER DATABASE foo CONFIGURE ZONE USING gc.ttlseconds = 12345;

-- Test 44: statement (line 415)
-- CREATE TABLE foo.public.bar (x INT PRIMARY KEY);

-- Test 45: statement (line 419)
-- INSERT INTO foo.public.bar VALUES (1);

-- Test 46: statement (line 422)
-- DROP DATABASE foo CASCADE;

-- Test 47: statement (line 425)
CREATE TABLE baz (x INT PRIMARY KEY);

-- Test 48: statement (line 429)
INSERT INTO baz VALUES (1);

-- Test 49: statement (line 432)
DROP TABLE baz;

-- Test 50: query (line 435)
-- SELECT name, ttl FROM crdb_internal.kv_dropped_relations ORDER BY name;

-- Test 51: statement (line 443)
-- ALTER RANGE default CONFIGURE ZONE DISCARD;

-- Test 52: statement (line 450)
-- CREATE DATABASE foo;

-- Test 53: statement (line 453)
-- ALTER RANGE default CONFIGURE ZONE USING gc.ttlseconds = 3;
-- ALTER RANGE default CONFIGURE ZONE USING gc.ttlseconds = 4;

-- Test 54: statement (line 456)
-- ALTER DATABASE foo CONFIGURE ZONE USING gc.ttlseconds = 3;
-- ALTER DATABASE foo CONFIGURE ZONE USING gc.ttlseconds = 4;

-- Test 55: statement (line 459)
-- ALTER DATABASE foo CONFIGURE ZONE DISCARD;
-- ALTER DATABASE foo CONFIGURE ZONE DISCARD;

-- Test 56: statement (line 466)
CREATE SEQUENCE seq
START WITH 1
INCREMENT BY 1
NO MINVALUE
NO MAXVALUE
CACHE 1;

-- Test 57: statement (line 474)
-- ALTER TABLE seq CONFIGURE ZONE USING gc.ttlseconds = 100000;

-- Test 58: query (line 495)
-- WITH settings AS (
--     SELECT
--       (crdb_internal.pb_to_json('cockroach.config.zonepb.ZoneConfig', config) -> 'numReplicas') AS replicas,
--       ((crdb_internal.pb_to_json('cockroach.config.zonepb.ZoneConfig', config) -> 'gc') ->> 'ttlSeconds')::INT AS ttl
--     FROM system.zones
--     -- 16 is the ID for the meta range
--     WHERE id = 16
-- )
-- SELECT *
-- FROM settings;

-- Test 59: statement (line 510)
-- RESET use_declarative_schema_changer;

-- Test 60: statement (line 519)
CREATE TABLE t(i INT PRIMARY KEY, j INT NOT NULL);
CREATE INDEX idx ON t(j);

-- skipif config local-legacy-schema-changer

-- Test 61: statement (line 523)
-- SET use_declarative_schema_changer = unsafe_always;

-- Test 62: statement (line 526)
BEGIN;
ALTER TABLE t DROP CONSTRAINT t_pkey;
ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (j);
COMMIT;

-- Test 63: query (line 532)
SELECT conname, pg_get_constraintdef(oid)
FROM pg_constraint
WHERE conrelid = 't'::regclass
  AND contype = 'p'
ORDER BY conname;

-- Test 64: query (line 555)
SELECT indexname, indexdef
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 't'
ORDER BY indexname;

-- Test 65: statement (line 585)
-- RESET use_declarative_schema_changer;

-- Test 66: statement (line 594)
CREATE TABLE foo(i int PRIMARY KEY);

-- Test 67: statement (line 597)
-- ALTER INDEX foo@foo_pkey CONFIGURE ZONE USING gc.ttlseconds=90;

-- Test 68: statement (line 600)
ALTER TABLE foo ADD COLUMN j INT NOT NULL DEFAULT 42;

-- Test 69: statement (line 607)
SELECT conname, pg_get_constraintdef(oid)
FROM pg_constraint
WHERE conrelid = 'foo'::regclass
  AND contype = 'p'
ORDER BY conname;

-- Test 70: query (line 644)
SELECT column_name, data_type, is_nullable
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'foo'
ORDER BY ordinal_position;

-- Test 71: query (line 667)
SELECT indexname, indexdef
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 'foo'
ORDER BY indexname;

-- Test 72: statement (line 685)
CREATE SEQUENCE seq1;

-- Test 73: statement (line 688)
-- ALTER TABLE seq1 CONFIGURE ZONE USING num_replicas=7;

-- Test 74: statement (line 706)
-- ALTER TABLE seq1 CONFIGURE ZONE DISCARD;

-- Test 75: statement (line 724)
DROP SEQUENCE seq1;

RESET client_min_messages;

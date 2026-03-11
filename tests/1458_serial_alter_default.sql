-- Serial columns: ALTER COLUMN SET/DROP DEFAULT (issue #1458)
-- Validates that SET DEFAULT overrides serial nextval, DROP DEFAULT removes it,
-- and sequence-based defaults can be restored.

-- 1. SET DEFAULT overrides serial nextval
CREATE TABLE t1458_set (id SERIAL PRIMARY KEY, v TEXT);
INSERT INTO t1458_set (v) VALUES ('before');
ALTER TABLE t1458_set ALTER COLUMN id SET DEFAULT 42;
INSERT INTO t1458_set (v) VALUES ('after');
SELECT id, v FROM t1458_set ORDER BY v;

-- 2. Explicit column value still works after SET DEFAULT
INSERT INTO t1458_set (id, v) VALUES (99, 'explicit');
SELECT id, v FROM t1458_set WHERE v = 'explicit';

-- 3. SET DEFAULT back to nextval restores sequence behavior
ALTER TABLE t1458_set ALTER COLUMN id SET DEFAULT nextval('public.t1458_set_id_seq');
INSERT INTO t1458_set (v) VALUES ('seq_restored');
SELECT id >= 1 AS valid_seq, v FROM t1458_set WHERE v = 'seq_restored';

-- 4. BIGSERIAL variant
CREATE TABLE t1458_big (id BIGSERIAL PRIMARY KEY, v TEXT);
ALTER TABLE t1458_big ALTER COLUMN id SET DEFAULT 42;
INSERT INTO t1458_big (v) VALUES ('big');
SELECT id, v FROM t1458_big;

-- 5. information_schema.columns reflects new default after SET DEFAULT
SELECT column_default FROM information_schema.columns
  WHERE table_name = 't1458_big' AND column_name = 'id';

-- 6. pg_attrdef reflects new default after SET DEFAULT
SELECT adsrc FROM pg_attrdef
  WHERE adrelid = (SELECT oid FROM pg_class WHERE relname = 't1458_big')
    AND adnum = 1;

-- 7. DROP DEFAULT on NOT NULL serial → INSERT without column → NOT NULL violation
CREATE TABLE t1458_drop (id SERIAL PRIMARY KEY, v TEXT);
ALTER TABLE t1458_drop ALTER COLUMN id DROP DEFAULT;
INSERT INTO t1458_drop (v) VALUES ('should_fail');

-- 8. SET DEFAULT 42, then DROP DEFAULT, then INSERT → NOT NULL violation (not revert to nextval)
CREATE TABLE t1458_drop2 (id SERIAL PRIMARY KEY, v TEXT);
ALTER TABLE t1458_drop2 ALTER COLUMN id SET DEFAULT 42;
ALTER TABLE t1458_drop2 ALTER COLUMN id DROP DEFAULT;
INSERT INTO t1458_drop2 (v) VALUES ('should_fail');

-- 9. Multi-row INSERT after SET DEFAULT
CREATE TABLE t1458_multi (id SERIAL PRIMARY KEY, v TEXT);
ALTER TABLE t1458_multi ALTER COLUMN id SET DEFAULT 42;
INSERT INTO t1458_multi (id, v) VALUES (1, 'a');
INSERT INTO t1458_multi (id, v) VALUES (2, 'b');
SELECT id, v FROM t1458_multi ORDER BY v;

-- 10. Unmodified serial regression guard (no ALTER)
CREATE TABLE t1458_unchanged (id SERIAL PRIMARY KEY, v TEXT);
INSERT INTO t1458_unchanged (v) VALUES ('x');
SELECT id, v FROM t1458_unchanged;

-- 11. SERIAL DEFAULT x rejected at CREATE TABLE
CREATE TABLE t1458_reject (id SERIAL DEFAULT 7);

-- Cleanup
DROP TABLE IF EXISTS t1458_set;
DROP TABLE IF EXISTS t1458_big;
DROP TABLE IF EXISTS t1458_drop;
DROP TABLE IF EXISTS t1458_drop2;
DROP TABLE IF EXISTS t1458_multi;
DROP TABLE IF EXISTS t1458_unchanged;
DROP TABLE IF EXISTS t1458_reject;

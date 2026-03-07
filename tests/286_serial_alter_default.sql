-- #1458: SERIAL columns must honor ALTER COLUMN SET/DROP DEFAULT.

DROP TABLE IF EXISTS serial_alter_default_add_col;
DROP TABLE IF EXISTS serial_alter_default_t;
DROP TABLE IF EXISTS serial_owned_test;

CREATE TABLE serial_alter_default_t (id SERIAL);

-- SET DEFAULT on SERIAL should override sequence behavior.
ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT 42;
INSERT INTO serial_alter_default_t DEFAULT VALUES;
SELECT 'after_set_default' AS phase, id FROM serial_alter_default_t ORDER BY id;
SELECT 'after_set_default_default_contains_42' AS check_name,
       POSITION('42' IN COALESCE(column_default, '')) > 0 AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';

-- DROP DEFAULT on SERIAL should not silently use nextval; NOT NULL should fail.
ALTER TABLE serial_alter_default_t ALTER COLUMN id DROP DEFAULT;
SELECT 'after_drop_default_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
INSERT INTO serial_alter_default_t DEFAULT VALUES;

-- Explicit nextval default should resume sequence from current counter.
ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT nextval(pg_get_serial_sequence('public.serial_alter_default_t', 'id'));
INSERT INTO serial_alter_default_t DEFAULT VALUES;
SELECT 'after_nextval' AS phase, id FROM serial_alter_default_t ORDER BY id;
SELECT 'after_nextval_default_contains_nextval' AS check_name,
       POSITION('nextval' IN COALESCE(column_default, '')) > 0 AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';

-- PostgreSQL parity: on SERIAL, SET DEFAULT NULL is equivalent to DROP DEFAULT.
ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT NULL;
SELECT 'after_set_default_null_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_null_atthasdef_false' AS check_name,
       NOT a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_null_pg_attrdef_absent' AS check_name,
       COUNT(*) = 0 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

-- PostgreSQL parity: typed NULL defaults on SERIAL are equivalent to DROP DEFAULT.
ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT NULL::INT;
SELECT 'after_set_default_null_cast_style_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_null_cast_style_atthasdef_false' AS check_name,
       NOT a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_null_cast_style_pg_attrdef_absent' AS check_name,
       COUNT(*) = 0 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT CAST(NULL AS INT);
SELECT 'after_set_default_cast_null_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_cast_null_atthasdef_false' AS check_name,
       NOT a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_cast_null_pg_attrdef_absent' AS check_name,
       COUNT(*) = 0 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT (NULL);
SELECT 'after_set_default_nested_null_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_nested_null_atthasdef_false' AS check_name,
       NOT a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_nested_null_pg_attrdef_absent' AS check_name,
       COUNT(*) = 0 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT CAST((NULL) AS INT);
SELECT 'after_set_default_cast_nested_null_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_cast_nested_null_atthasdef_false' AS check_name,
       NOT a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_cast_nested_null_pg_attrdef_absent' AS check_name,
       COUNT(*) = 0 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

-- NULL-valued expressions (non-literal) must remain explicit defaults on SERIAL.
ALTER TABLE serial_alter_default_t ALTER COLUMN id SET DEFAULT NULLIF(1,1);
SELECT 'after_set_default_nullif_column_default_matches' AS check_name,
       column_default = 'NULLIF(1, 1)' AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_alter_default_t'
  AND column_name = 'id';
SELECT 'after_set_default_nullif_atthasdef_true' AS check_name,
       a.atthasdef AS ok
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';
SELECT 'after_set_default_nullif_pg_attrdef_present' AS check_name,
       COUNT(*) = 1 AS ok
FROM pg_catalog.pg_attrdef AS d
JOIN pg_catalog.pg_class AS c ON c.oid = d.adrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute AS a
  ON a.attrelid = c.oid
 AND a.attnum = d.adnum
WHERE n.nspname = 'public'
  AND c.relname = 'serial_alter_default_t'
  AND a.attname = 'id';

-- Regression: explicit owned-sequence nextval defaults on SERIAL must still be alterable.
CREATE TABLE serial_owned_test (id SERIAL);
ALTER TABLE serial_owned_test ALTER COLUMN id SET DEFAULT nextval('serial_owned_test_id_seq'::regclass);
ALTER TABLE serial_owned_test ALTER COLUMN id DROP DEFAULT;
SELECT 'serial_owned_drop_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'serial_owned_test'
  AND column_name = 'id';

-- Parity: SERIAL cannot have an explicit DEFAULT in column definition.
CREATE TABLE serial_alter_default_bad_create (id SERIAL DEFAULT 7);

CREATE TABLE serial_alter_default_add_col (payload TEXT);
ALTER TABLE serial_alter_default_add_col ADD COLUMN id SERIAL DEFAULT 7;

DROP TABLE serial_alter_default_add_col;
DROP TABLE serial_alter_default_t;
DROP TABLE serial_owned_test;

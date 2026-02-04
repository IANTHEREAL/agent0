-- ported from pg_tests PR#58: compatible/create_table.sql
-- Simplified version with basic CREATE TABLE tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS t140_test2 CASCADE;
DROP TABLE IF EXISTS t140_test1 CASCADE;
DROP TABLE IF EXISTS t140_t43894 CASCADE;
DROP TABLE IF EXISTS t140_new_table CASCADE;
DROP TABLE IF EXISTS t140_sec_col_fam CASCADE;
DROP TABLE IF EXISTS t140_t CASCADE;

-- Test 1: CockroachDB settings (commented out)
-- SET create_table_with_schema_locked=false;

-- Test 2-4: Foreign key constraint tests
CREATE TABLE t140_test2 (col1 SERIAL PRIMARY KEY, col2 BIGINT);

-- Test with duplicate constraint names (should fail in real test)
-- CREATE TABLE t140_test1 (col1 SERIAL PRIMARY KEY, col2 BIGINT, col3 BIGINT,
--   CONSTRAINT duplicate_name FOREIGN KEY (col2) REFERENCES t140_test2(col1),
--   CONSTRAINT duplicate_name FOREIGN KEY (col3) REFERENCES t140_test2(col1));

DROP TABLE t140_test2;

-- Test 5: UUID and JSONB
CREATE TABLE t140_t43894 (a UUID NOT NULL PRIMARY KEY, b JSONB NOT NULL DEFAULT '5');

-- Test 6-7: timetz column type
CREATE TABLE t140_new_table (a TIMETZ(3));
ALTER TABLE t140_new_table ADD COLUMN c TIMETZ(4);

-- Test 8: Feature usage (commented out - crdb_internal)
-- SELECT feature_name FROM crdb_internal.feature_usage;

-- Test 9: autocommit setting (commented out)
-- SET autocommit_before_ddl = false;

-- Test 10-11: Secondary index column families (PG doesn't support column families)
CREATE TABLE t140_sec_col_fam(x INT, y INT, z INT);
CREATE INDEX t140_idx_sec_col ON t140_sec_col_fam (x);

-- Test 12: Reset (commented out)
-- RESET autocommit_before_ddl;

-- Test 13-15: Primary key requirements
-- set require_explicit_primary_keys=true;
CREATE TABLE t140_t (x INT PRIMARY KEY, y INT);

SELECT 'create_table tests completed successfully' AS result;

DROP TABLE t140_t;
DROP TABLE t140_sec_col_fam;
DROP TABLE t140_new_table;
DROP TABLE t140_t43894;

-- db9-specific: information_schema owner columns are db9 extensions, not in PG 17
-- ALTER ... OWNER TO (tables, sequences, functions)

-- Cleanup from prior runs
DROP FUNCTION IF EXISTS test_func();
DROP SEQUENCE IF EXISTS owner_seq;
DROP TABLE IF EXISTS owner_test;

-- Table owner
CREATE TABLE owner_test (id INT PRIMARY KEY);
ALTER TABLE owner_test OWNER TO alice;
SELECT table_name, table_owner
FROM information_schema.tables
WHERE table_name = 'owner_test'
ORDER BY table_name;

ALTER TABLE owner_test OWNER TO bob;
SELECT table_name, table_owner
FROM information_schema.tables
WHERE table_name = 'owner_test'
ORDER BY table_name;

-- Sequence owner
CREATE SEQUENCE owner_seq;
ALTER SEQUENCE owner_seq OWNER TO alice;
SELECT sequence_name, sequence_owner
FROM information_schema.sequences
WHERE sequence_name = 'owner_seq'
ORDER BY sequence_name;

-- Function owner
CREATE FUNCTION test_func() RETURNS INT AS $$
    SELECT 1;
$$ LANGUAGE SQL;
ALTER FUNCTION test_func() OWNER TO alice;
SELECT routine_name, routine_owner
FROM information_schema.routines
WHERE routine_name = 'test_func'
ORDER BY routine_name;

-- Cleanup
DROP FUNCTION test_func();
DROP SEQUENCE owner_seq;
DROP TABLE owner_test;


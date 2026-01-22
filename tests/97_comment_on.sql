-- COMMENT ON ... persistence via pg_catalog.pg_description

-- Cleanup from prior runs
DROP FUNCTION IF EXISTS comment_test_fn();
DROP TABLE IF EXISTS comment_test;
DROP EXTENSION IF EXISTS "uuid-ossp";

-- Extension comment
CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA public;
COMMENT ON EXTENSION "uuid-ossp" IS 'ext comment';

-- Column comment
CREATE TABLE comment_test (id INT PRIMARY KEY, name TEXT);
COMMENT ON COLUMN comment_test.name IS 'col comment';
COMMENT ON COLUMN comment_test.name IS 'col comment v2';

-- Function comment
CREATE FUNCTION comment_test_fn() RETURNS INT AS $$
    SELECT 1;
$$ LANGUAGE SQL;
COMMENT ON FUNCTION comment_test_fn() IS 'fn comment';

-- Validate comments via pg_description joins (no hard-coded OIDs).
SELECT e.extname, d.description
FROM pg_catalog.pg_extension e
JOIN pg_catalog.pg_description d ON d.objoid = e.oid
WHERE e.extname = 'uuid-ossp'
ORDER BY e.extname;

SELECT c.relname, a.attname, d.description
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = a.attnum
WHERE c.relname = 'comment_test' AND a.attname = 'name'
ORDER BY a.attname;

SELECT p.proname, d.description
FROM pg_catalog.pg_proc p
JOIN pg_catalog.pg_description d ON d.objoid = p.oid
WHERE p.proname = 'comment_test_fn'
ORDER BY p.proname;

-- Drop comment and verify it disappears.
COMMENT ON COLUMN comment_test.name IS NULL;
SELECT COUNT(*) AS comment_rows
FROM pg_catalog.pg_description d
JOIN pg_catalog.pg_class c ON c.oid = d.objoid
JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.objsubid
WHERE c.relname = 'comment_test' AND a.attname = 'name';

-- Cleanup
DROP FUNCTION comment_test_fn();
DROP TABLE comment_test;
DROP EXTENSION IF EXISTS "uuid-ossp";

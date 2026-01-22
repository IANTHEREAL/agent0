-- Test three-part identifiers (schema.table.column) in JOIN conditions
-- This is needed for PostgreSQL compatibility (e.g., pg_catalog.pg_class.relname)

-- Test 1: Three-part identifier in JOIN ON clause with pg_catalog tables
SELECT pg_catalog.pg_class.relname
FROM pg_catalog.pg_class
JOIN pg_catalog.pg_namespace
  ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace
WHERE pg_catalog.pg_namespace.nspname = 'public'
ORDER BY pg_catalog.pg_class.relname
LIMIT 5;

-- Test 2: Mixed two-part and three-part identifiers
SELECT pg_class.relname, pg_catalog.pg_namespace.nspname
FROM pg_catalog.pg_class
JOIN pg_catalog.pg_namespace
  ON pg_catalog.pg_namespace.oid = pg_class.relnamespace
WHERE pg_namespace.nspname = 'public'
ORDER BY relname
LIMIT 5;

-- Test 3: Three-part identifier without aliases
SELECT pg_catalog.pg_class.relname
FROM pg_catalog.pg_class
JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace
WHERE pg_catalog.pg_namespace.nspname = 'public'
ORDER BY pg_catalog.pg_class.relname
LIMIT 3;

-- Test 4: Create user tables and test with user schema
CREATE SCHEMA IF NOT EXISTS test_schema;
CREATE TABLE test_schema.items (id INT PRIMARY KEY, name TEXT);
CREATE TABLE test_schema.categories (id INT PRIMARY KEY, cat_name TEXT);
INSERT INTO test_schema.items VALUES (1, 'Item1'), (2, 'Item2');
INSERT INTO test_schema.categories VALUES (1, 'Cat1'), (2, 'Cat2');

-- Test 5: Three-part identifier with user tables
SELECT test_schema.items.name, test_schema.categories.cat_name
FROM test_schema.items
JOIN test_schema.categories
  ON test_schema.items.id = test_schema.categories.id
ORDER BY test_schema.items.id;

-- Cleanup
DROP TABLE test_schema.items;
DROP TABLE test_schema.categories;
DROP SCHEMA test_schema;

-- Test 6: pg_table_is_visible in JOIN WHERE clause (Dify compatibility)
SELECT pg_catalog.pg_class.relname
FROM pg_catalog.pg_class
JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace
WHERE pg_catalog.pg_class.relkind = 'r'
  AND pg_catalog.pg_table_is_visible(pg_catalog.pg_class.oid)
ORDER BY pg_catalog.pg_class.relname
LIMIT 3;

-- Test 7: Simple scalar subquery in JOIN SELECT
CREATE TABLE subq_a (aid INT PRIMARY KEY, val INT);
CREATE TABLE subq_b (bid INT PRIMARY KEY, aid INT);
INSERT INTO subq_a VALUES (1, 100), (2, 200);
INSERT INTO subq_b VALUES (10, 1), (20, 2);

SELECT a.aid,
       (SELECT COUNT(*) FROM subq_b WHERE subq_b.aid = a.aid) AS cnt
FROM subq_a a
JOIN subq_b b ON b.aid = a.aid
ORDER BY a.aid;

DROP TABLE subq_b;
DROP TABLE subq_a;

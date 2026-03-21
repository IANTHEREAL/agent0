-- to_regclass(text): PostgreSQL-compatible relation existence check
-- Issue #1628

-- Setup
CREATE TABLE regclass_test (id int PRIMARY KEY, val text);

-- 1. Schema-qualified table lookup
SELECT to_regclass('public.regclass_test') IS NOT NULL AS found;

-- 2. Non-existent relation returns NULL
SELECT to_regclass('public.nonexistent') IS NULL AS is_null;

-- 3. Unqualified search_path resolution
SELECT to_regclass('regclass_test') IS NOT NULL AS found;

-- 4. pg_catalog-qualified call
SELECT pg_catalog.to_regclass('public.regclass_test') IS NOT NULL AS found;

-- 5. NULL input returns NULL
SELECT to_regclass(NULL) IS NULL AS is_null;

-- 6. OID consistency with pg_class
SELECT to_regclass('public.regclass_test') = (SELECT oid FROM pg_class WHERE relname = 'regclass_test' AND relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'public')) AS oid_match;

-- 7. Index lookup
CREATE INDEX regclass_test_idx ON regclass_test (val);
SELECT to_regclass('regclass_test_idx') IS NOT NULL AS found;

-- 8. View lookup
CREATE VIEW regclass_test_v AS SELECT 1 AS x;
SELECT to_regclass('regclass_test_v') IS NOT NULL AS found;

-- 9. Sequence lookup
CREATE SEQUENCE regclass_test_seq;
SELECT to_regclass('regclass_test_seq') IS NOT NULL AS found;

-- 10. Materialized view lookup (resolves via table path in db9)
CREATE MATERIALIZED VIEW regclass_test_mv AS SELECT 1 AS x;
SELECT to_regclass('regclass_test_mv') IS NOT NULL AS found;

-- 11. Quoted case-sensitive identifier
CREATE TABLE "CaseSensitive" (id int);
SELECT to_regclass('"CaseSensitive"') IS NOT NULL AS found;

-- 12. Unquoted lowercased does not match case-sensitive name
SELECT to_regclass('casesensitive') IS NULL AS is_null;

-- 13. Multi-schema search_path precedence
CREATE SCHEMA sp2;
CREATE TABLE sp2.t1 (id int);
SET search_path = sp2, public;
SELECT to_regclass('t1') IS NOT NULL AS found;
RESET search_path;

-- 14. Current-database-qualified text is accepted for to_regclass
SELECT to_regclass('"' || current_database() || '"."public"."regclass_test"') IS NOT NULL AS found;

-- 15. Current-database-qualified text cast to regclass resolves to the same OID
SELECT (('"' || current_database() || '"."public"."regclass_test"')::regclass::oid = (SELECT oid FROM pg_class WHERE relname = 'regclass_test' AND relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'public'))) AS oid_match;

-- 16. Wrong arg type produces error
SELECT to_regclass(42);

-- 17. Three-part name (cross-database reference) produces error
SELECT to_regclass('a.b.c');

-- 18. Non-pg_catalog schema-qualified call produces error
SELECT public.to_regclass('regclass_test');

-- Cleanup
DROP TABLE IF EXISTS sp2.t1;
DROP SCHEMA IF EXISTS sp2;
DROP MATERIALIZED VIEW IF EXISTS regclass_test_mv;
DROP SEQUENCE IF EXISTS regclass_test_seq;
DROP VIEW IF EXISTS regclass_test_v;
DROP TABLE IF EXISTS "CaseSensitive";
DROP TABLE IF EXISTS regclass_test;

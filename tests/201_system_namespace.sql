-- PostgreSQL compatible tests from system_namespace
-- 2 tests

SET client_min_messages = warning;

-- CockroachDB exposes `system.namespace`; model the minimal shape needed by this
-- test so the queries remain meaningful under PostgreSQL.
DROP SCHEMA IF EXISTS system CASCADE;
CREATE SCHEMA system;
CREATE TABLE system.namespace (
    "parentID" INT NOT NULL,
    "parentSchemaID" INT NOT NULL,
    name TEXT NOT NULL,
    id INT NOT NULL
);

INSERT INTO system.namespace ("parentID", "parentSchemaID", name, id) VALUES
    (0, 0, 'comments', 50),
    (0, 0, 'locations', 51),
    (0, 0, 'descriptor_id_seq', 52),
    (0, 0, 'userobj1', 100),
    (0, 0, 'userobj2', 101);

-- Test 1: query (line 1)
SELECT *
FROM system.namespace
WHERE id >= 100 OR name IN ('comments', 'locations', 'descriptor_id_seq')
ORDER BY id, name;

-- Test 2: query (line 15)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'system'
  AND table_name = 'namespace'
ORDER BY ordinal_position;

RESET client_min_messages;

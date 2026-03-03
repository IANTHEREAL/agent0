-- PostgreSQL compatible tests from sql_keys
-- 11 tests

-- CockroachDB's KV-layer key inspection helpers (crdb_internal.*) do not exist in
-- PostgreSQL. Keep this test file runnable by emitting a stable marker row and
-- leaving the original statements as comments for reference.
SELECT 'SKIP: crdb_internal SQL key inspection is CockroachDB-specific' AS note;

-- Test 1: query (line 27)
-- SELECT key FROM crdb_internal.list_sql_keys_in_range($rangeid)

-- Test 2: statement (line 32)
-- INSERT INTO t VALUES (1, 1), (2, 2)

-- Test 3: query (line 38)
-- SELECT key FROM crdb_internal.list_sql_keys_in_range($rangeid)

-- Test 4: query (line 45)
-- SELECT key FROM crdb_internal.list_sql_keys_in_range($rangeid)

-- Test 5: query (line 54)
-- SELECT crdb_internal.pretty_key(key, 0) FROM crdb_internal.scan(crdb_internal.table_span($tableid))

-- Test 6: query (line 61)
-- SELECT crdb_internal.pretty_key(key, 0) FROM crdb_internal.scan(crdb_internal.table_span($tableid))

-- Test 7: statement (line 68)
-- SELECT
-- 	crdb_internal.pretty_key(key, 0)
-- FROM
-- 	crdb_internal.scan(
-- 		ARRAY[
-- 			crdb_internal.table_span($tableid)[1],
-- 			(
-- 				SELECT key || '\x00'::BYTES
-- 				FROM crdb_internal.scan(crdb_internal.table_span($tableid))
-- 				ORDER BY key DESC
-- 				LIMIT 1
-- 			)
-- 		]
-- 	)

-- Test 8: statement (line 85)
-- SELECT key FROM crdb_internal.list_sql_keys_in_range(1000000)

-- Test 9: query (line 101)
-- SELECT count(key), count(DISTINCT key) FROM crdb_internal.list_sql_keys_in_range($rangeid)

-- Test 10: query (line 106)
-- SELECT count(key), count(DISTINCT key) FROM crdb_internal.scan(crdb_internal.table_span($tableid))

-- Test 11: statement (line 118)
-- SELECT crdb_internal.scan('\xff':::BYTES, '\x3f5918':::BYTES);

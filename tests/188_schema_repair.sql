-- PostgreSQL compatible tests from schema_repair
-- 59 tests
--
-- PG-NOT-SUPPORTED: CockroachDB schema repair tests depend on internal
-- descriptor/catalog APIs (system.namespace/system.descriptor/crdb_internal.*)
-- that do not exist in PostgreSQL.
--
-- The original CockroachDB-derived SQL is preserved below for reference, but is
-- not executed under PostgreSQL.

SET client_min_messages = warning;

SELECT
  'skipped: schema_repair requires CockroachDB crdb_internal/system catalog APIs'
    AS notice;

RESET client_min_messages;

/*
-- PostgreSQL compatible tests from schema_repair
-- 59 tests

-- Test 1: statement (line 8)
SET CLUSTER SETTING sql.catalog.descriptor_wait_for_initial_version.enabled=false

-- Test 2: statement (line 11)
CREATE TABLE corruptdesc (v INT8)

-- Test 3: statement (line 14)
CREATE TABLE lostdata (v INT8)

-- Test 4: statement (line 17)
INSERT INTO lostdata VALUES (3);

-- Test 5: statement (line 20)
INSERT INTO lostdata VALUES (5);

-- Test 6: statement (line 23)
INSERT INTO lostdata VALUES (23);

let $t_id
SELECT id FROM system.namespace WHERE name = 'lostdata';

let $corrupt_id
SELECT id FROM system.namespace WHERE name = 'corruptdesc';

let $parentID
SELECT pid FROM system.namespace AS n(pid,psid,name,id) WHERE id = $t_id;

let $parentSchemaID
SELECT psid FROM system.namespace AS n(pid,psid,name,id) WHERE id = $t_id;

-- Test 7: query (line 38)
SELECT * FROM crdb_internal.lost_descriptors_with_data;

-- Test 8: query (line 87)
SELECT * FROM ROWS FROM (crdb_internal.unsafe_delete_descriptor($corrupt_id));

-- Test 9: query (line 93)
SELECT * FROM ROWS FROM (crdb_internal.unsafe_delete_descriptor($t_id));

-- Test 10: statement (line 103)
SELECT * FROM crdb_internal.unsafe_upsert_descriptor($corrupt_id, crdb_internal.json_to_pb( 'cockroach.sql.sqlbase.Descriptor','$json_t_corrupt'), true)

-- Test 11: query (line 106)
SELECT count(*) FROM crdb_internal.lost_descriptors_with_data WHERE descid = $t_id;

-- Test 12: query (line 111)
SELECT count(*) FROM crdb_internal.lost_descriptors_with_data WHERE descid != $t_id

-- Test 13: statement (line 116)
SELECT * FROM crdb_internal.unsafe_upsert_descriptor($t_id, crdb_internal.json_to_pb( 'cockroach.sql.sqlbase.Descriptor','$json_t'))

-- Test 14: statement (line 120)
SELECT * FROM crdb_internal.unsafe_upsert_descriptor($corrupt_id, crdb_internal.json_to_pb( 'cockroach.sql.sqlbase.Descriptor','$json_corrupt'), true)

-- Test 15: statement (line 123)
SELECT * FROM corruptdesc;

-- Test 16: statement (line 126)
DROP TABLE lostdata

-- Test 17: statement (line 132)
CREATE TABLE forcedeletemydata (v int)

-- Test 18: statement (line 135)
INSERT INTO forcedeletemydata VALUES(5)

-- Test 19: statement (line 138)
INSERT INTO forcedeletemydata VALUES(7)

-- Test 20: query (line 141)
SELECT * FROM forcedeletemydata ORDER BY v ASC

-- Test 21: statement (line 148)
select * from crdb_internal.force_delete_table_data(6666)


let $t_id
select id from system.namespace where name='forcedeletemydata'

-- Test 22: statement (line 156)
select * from crdb_internal.force_delete_table_data($t_id)

-- Test 23: query (line 159)
SELECT * FROM forcedeletemydata ORDER BY v ASC

-- Test 24: query (line 184)
select * from crdb_internal.unsafe_delete_descriptor($t_id);

-- Test 25: query (line 189)
select * from crdb_internal.force_delete_table_data($t_id)

-- Test 26: statement (line 194)
select * from crdb_internal.unsafe_upsert_descriptor($t_id, crdb_internal.json_to_pb( 'cockroach.sql.sqlbase.Descriptor','$json'))

-- Test 27: query (line 197)
SELECT * FROM forcedeletemydata ORDER BY v ASC

-- Test 28: statement (line 201)
DROP TABLE forcedeletemydata

-- Test 29: query (line 212)
SELECT
	crdb_internal.unsafe_delete_descriptor(id),
	crdb_internal.unsafe_delete_namespace_entry("parentID", "parentSchemaID", name, id)
FROM
	system.namespace
WHERE
	name = 'corrupt_fk'

-- Test 30: query (line 223)
SELECT * FROM corrupt_backref_fk

-- Test 31: statement (line 228)
DROP TABLE corrupt_backref_fk

-- Test 32: query (line 236)
SELECT
	crdb_internal.unsafe_delete_descriptor(id),
	crdb_internal.unsafe_delete_namespace_entry("parentID", "parentSchemaID", name, id)
FROM
	system.namespace
WHERE
	name = 'corrupt_view'

-- Test 33: query (line 247)
SELECT * FROM corrupt_backref_view

-- Test 34: statement (line 254)
DROP TABLE corrupt_backref_view

skipif config local-legacy-schema-changer

-- Test 35: statement (line 258)
DROP TABLE corrupt_backref_view

-- Test 36: statement (line 261)
CREATE TYPE corrupt_backref_typ AS ENUM ('a', 'b');
CREATE TABLE corrupt_typ (k INT PRIMARY KEY, v corrupt_backref_typ);

-- Test 37: query (line 265)
SELECT
	crdb_internal.unsafe_delete_descriptor(id),
	crdb_internal.unsafe_delete_namespace_entry("parentID", "parentSchemaID", name, id)
FROM
	system.namespace
WHERE
	name = 'corrupt_typ'

-- Test 38: query (line 276)
SELECT 'a'::corrupt_backref_typ

-- Test 39: statement (line 281)
ALTER TYPE corrupt_backref_typ DROP VALUE 'b'

-- Test 40: query (line 287)
SELECT crdb_internal.unsafe_upsert_namespace_entry(104,105,'dangling',12345,true)
UNION ALL
SELECT crdb_internal.unsafe_upsert_namespace_entry(104,105,'dangling2',12345,true)

-- Test 41: query (line 295)
SELECT * FROM "".crdb_internal.kv_repairable_catalog_corruptions ORDER BY 1,2,3,4

-- Test 42: query (line 305)
SELECT id, corruption, crdb_internal.repair_catalog_corruption(id, corruption)
FROM "".crdb_internal.kv_repairable_catalog_corruptions
ORDER BY 1

-- Test 43: statement (line 326)
SELECT
	crdb_internal.unsafe_upsert_descriptor(
		d.id,
		crdb_internal.json_to_pb(
			'cockroach.sql.sqlbase.Descriptor',
			json_set(
				crdb_internal.pb_to_json('cockroach.sql.sqlbase.Descriptor', d.descriptor),
				ARRAY['table', 'nextColumnId'],
				'1'::JSONB
			)
		),
		true
	)
FROM
	system.descriptor AS d INNER JOIN system.namespace AS ns ON d.id = ns.id
WHERE
	name = 'kv'

-- Test 44: statement (line 345)
ALTER TABLE kv RENAME TO kv

-- Test 45: statement (line 348)
SET descriptor_validation = off

-- Test 46: statement (line 351)
ALTER TABLE kv RENAME TO kv

-- Test 47: statement (line 354)
SET descriptor_validation = on

-- Test 48: statement (line 358)
SELECT
	crdb_internal.unsafe_upsert_descriptor(
		d.id,
		crdb_internal.json_to_pb(
			'cockroach.sql.sqlbase.Descriptor',
			json_set(
				crdb_internal.pb_to_json('cockroach.sql.sqlbase.Descriptor', d.descriptor),
				ARRAY['table', 'nextColumnId'],
				'3'::JSONB
			)
		),
		true
	)
FROM
	system.descriptor AS d INNER JOIN system.namespace AS ns ON d.id = ns.id
WHERE
	name = 'kv'

-- Test 49: statement (line 377)
DROP TABLE kv

-- Test 50: statement (line 404)
CREATE TABLE t (a INT PRIMARY KEY, b INT, INDEX (b))

-- Test 51: query (line 407)
SELECT stripped(tbl_json(desc_bytes('t')))

-- Test 52: query (line 413)
SELECT stripped(tbl_json(crdb_internal.descriptor_with_post_deserialization_changes(desc_bytes('t'))))

-- Test 53: statement (line 420)
SELECT
	crdb_internal.unsafe_upsert_descriptor(
		d.id,
		crdb_internal.json_to_pb(
			'cockroach.sql.sqlbase.Descriptor',
			json_set(
				crdb_internal.pb_to_json('cockroach.sql.sqlbase.Descriptor', d.descriptor),
				'{table, indexes, 0, notVisible}',
				'true'::JSONB
			)
		),
		true
	)
FROM
	system.descriptor AS d INNER JOIN system.namespace AS ns ON d.id = ns.id
WHERE
	name = 't'

-- Test 54: query (line 439)
SELECT stripped(tbl_json(desc_bytes('t')))

-- Test 55: query (line 444)
SELECT stripped(tbl_json(crdb_internal.descriptor_with_post_deserialization_changes(desc_bytes('t'))))

-- Test 56: query (line 450)
SELECT
	stripped(
		tbl_json(
			crdb_internal.descriptor_with_post_deserialization_changes(
				crdb_internal.json_to_pb(
					'cockroach.sql.sqlbase.Descriptor',
					json_set(
						crdb_internal.pb_to_json(
							'cockroach.sql.sqlbase.Descriptor',
							desc_bytes('t')
						),
						ARRAY['table', 'modificationTime'],
						'{\"logical\": 1, \"wallTime\": \"123456789\"}'::JSONB
					)
				)
			)
		)
	)

-- Test 57: statement (line 473)
ALTER TABLE t RENAME TO t_renamed

-- Test 58: query (line 476)
SELECT stripped(tbl_json(desc_bytes('t_renamed')))

-- Test 59: statement (line 481)
DROP TABLE t_renamed
*/


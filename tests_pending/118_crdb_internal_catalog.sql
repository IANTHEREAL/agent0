-- PostgreSQL compatible tests from crdb_internal_catalog
-- 20 tests

\set ON_ERROR_STOP 1

-- Test 1: statement (line 6)
-- CockroachDB-only (and would leak a DB outside the isolated test database).
-- CREATE DATABASE db;

-- Test 2: statement (line 9)
-- CockroachDB-only.
-- ALTER DATABASE db CONFIGURE ZONE USING gc.ttlseconds = 7200;

-- Test 3: statement (line 12)
-- CockroachDB-only.
-- USE db;

-- Test 4: statement (line 15)
CREATE SCHEMA sc;

-- Test 5: statement (line 18)
CREATE TYPE sc.greeting AS ENUM('hi', 'hello');

-- PostgreSQL setup: kv is referenced by later statements in this file.
CREATE TABLE kv (k INT PRIMARY KEY, v INT);

-- Test 6: statement (line 24)
ALTER TABLE kv ADD CONSTRAINT ck CHECK (k > 0);

-- Test 7: statement (line 27)
CREATE MATERIALIZED VIEW mv AS SELECT k, v FROM kv;

-- Test 8: statement (line 30)
CREATE INDEX idx ON mv(v);

-- Test 9: statement (line 33)
-- CockroachDB-only.
-- ALTER TABLE kv CONFIGURE ZONE USING gc.ttlseconds = 3600;

-- Test 10: statement (line 36)
SELECT current_database() AS dbname \gset
COMMENT ON DATABASE :"dbname" IS 'this is the test database';

-- Test 11: statement (line 39)
COMMENT ON SCHEMA sc IS 'this is a schema';

-- Test 12: statement (line 42)
COMMENT ON SCHEMA public IS 'this is the public schema';

-- Test 13: statement (line 45)
COMMENT ON TABLE kv IS 'this is a table';

-- Test 14: statement (line 48)
COMMENT ON INDEX idx IS 'this is an index';

-- Test 15: statement (line 51)
COMMENT ON CONSTRAINT ck ON kv IS 'this is a check constraint';

-- Test 16: statement (line 54)
COMMENT ON CONSTRAINT kv_pkey ON kv IS 'this is a primary key constraint';

-- Test 17: statement (line 57)
-- CockroachDB-only.
-- USE test;

-- Test 18: statement (line 60)
CREATE OR REPLACE FUNCTION json_remove_path(d JSONB, path TEXT[])
  RETURNS JSONB
  IMMUTABLE
  LANGUAGE SQL
AS $$
  SELECT d #- path
$$;

CREATE FUNCTION strip_volatile(IN d JSONB)
	RETURNS JSONB
	STABLE
	LANGUAGE SQL
	AS $$
SELECT
	json_remove_path(
		json_remove_path(
			json_remove_path(
				json_remove_path(
					json_remove_path(
						json_remove_path(
							json_remove_path(
								json_remove_path(
									json_remove_path(
										json_remove_path(
											json_remove_path(
												json_remove_path(
													json_remove_path(
														json_remove_path(d, ARRAY['table', 'families']),
														ARRAY['table', 'nextFamilyId']
													),
													ARRAY['table', 'id']
												),
												ARRAY['table', 'unexposedParentSchemaId']
											),
											ARRAY['table', 'indexes', '0', 'createdAtNanos']
										),
										ARRAY['table', 'indexes', '1', 'createdAtNanos']
									),
									ARRAY['table', 'indexes', '2', 'createdAtNanos']
								),
								ARRAY['table', 'primaryIndex', 'createdAtNanos']
							),
							ARRAY['table', 'createAsOfTime']
						),
						ARRAY['table', 'modificationTime']
					),
					ARRAY['function', 'modificationTime']
				),
				ARRAY['type', 'modificationTime']
			),
			ARRAY['schema', 'modificationTime']
		),
		ARRAY['database', 'modificationTime']
	)
	$$;

-- Test 19: query (line 116)
CREATE SCHEMA crdb_internal;
CREATE TABLE crdb_internal.kv_catalog_descriptor (
  id INT PRIMARY KEY,
  descriptor JSONB NOT NULL
);

-- Use a CRDB-like descriptor shape so strip_volatile exercises the same JSON paths.
INSERT INTO crdb_internal.kv_catalog_descriptor (id, descriptor) VALUES
  (1, jsonb_build_object(
    'table', jsonb_build_object(
      'id', 1,
      'families', jsonb_build_array(jsonb_build_object('id', 0)),
      'nextFamilyId', 1,
      'unexposedParentSchemaId', 0,
      'indexes', jsonb_build_array(
        jsonb_build_object('createdAtNanos', 1),
        jsonb_build_object('createdAtNanos', 2),
        jsonb_build_object('createdAtNanos', 3)
      ),
      'primaryIndex', jsonb_build_object('createdAtNanos', 4),
      'createAsOfTime', '2000-01-01T00:00:00Z',
      'modificationTime', '2000-01-01T00:00:00Z'
    ),
    'database', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'),
    'schema', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'),
    'type', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'),
    'function', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z')
  )),
  (2, jsonb_build_object('table', jsonb_build_object('id', 2, 'indexes', jsonb_build_array(jsonb_build_object('createdAtNanos', 1), jsonb_build_object('createdAtNanos', 2), jsonb_build_object('createdAtNanos', 3))))),
  (3, jsonb_build_object('type', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'))),
  (29, jsonb_build_object('schema', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'))),
  (150, jsonb_build_object('database', jsonb_build_object('modificationTime', '2000-01-01T00:00:00Z'))),
  ('geometry_columns'::regclass::int, jsonb_build_object('table', jsonb_build_object('id', 'geometry_columns'::regclass::int)));

SELECT least(id, 999), strip_volatile(descriptor)
FROM crdb_internal.kv_catalog_descriptor
WHERE id IN (1, 2, 3, 29, 'geometry_columns'::regclass::int) OR (id > 100 and id < 200)
ORDER BY id;

-- Test 20: query (line 138)
SELECT least(id, 999), strip_volatile(descriptor)
FROM crdb_internal.kv_catalog_descriptor
WHERE id IN (1, 2, 3, 29, 'geometry_columns'::regclass::int) OR (id > 100 and id < 200)
ORDER BY id;

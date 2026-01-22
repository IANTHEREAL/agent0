-- GIN Index Query Tests (JSONB @>)
-- Purpose: Verify GIN backfill + planner selection for JSONB containment queries.

DROP TABLE IF EXISTS gin_test;

CREATE TABLE gin_test (
    id INT PRIMARY KEY,
    metadata JSONB
);

INSERT INTO gin_test (id, metadata) VALUES
  (1, '{"type":"pdf","author":{"name":"user_1"}}'),
  (2, '{"type":"doc","author":{"name":"user_2"}}'),
  (3, '{"type":"pdf","author":{"name":"user_3"}}'),
  (4, '{"n":1}');

CREATE INDEX gin_test_metadata_idx ON gin_test USING gin (metadata);

-- EXPLAIN should show an index scan (GIN-like inverted index).
EXPLAIN SELECT * FROM gin_test WHERE metadata @> '{"type":"pdf"}';

-- Correctness checks (deterministic order).
SELECT id FROM gin_test WHERE metadata @> '{"type":"pdf"}' ORDER BY id;
SELECT id FROM gin_test WHERE metadata @> '{"author":{"name":"user_1"}}' ORDER BY id;
SELECT COUNT(*) FROM gin_test WHERE metadata @> '{"nonexistent": true}';

-- Numeric canonicalization (1 == 1.0 for JSONB numeric equality).
SELECT id FROM gin_test WHERE metadata @> '{"n":1.0}' ORDER BY id;

DROP TABLE gin_test;


-- PR #2053: HNSW two-level search (base graph + delta index)
-- Tests delta precedence, DELETE visibility, multiple UPDATEs.

-- ================================================================
-- Setup
-- ================================================================
DROP TABLE IF EXISTS hnsw_2l;
CREATE TABLE hnsw_2l (id INT PRIMARY KEY, v VECTOR(3));
-- id=2 is deliberately closer to the x-axis than id=3 to break ties
-- deterministically when id=1 is moved away from [1,0,0].
INSERT INTO hnsw_2l (id, v) VALUES
    (1, '[0.9, 0.0, 0.0]'),
    (2, '[0.5, 0.0, 0.0]'),
    (3, '[0.0, 0.0, 0.9]');
CREATE INDEX idx_2l ON hnsw_2l USING hnsw (v vector_l2_ops);

-- ================================================================
-- Test 1: Baseline
-- ================================================================
SELECT 'baseline' AS test, id FROM hnsw_2l ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ================================================================
-- Test 2: UPDATE moves vector, delta overrides base graph
-- ================================================================
UPDATE hnsw_2l SET v = '[0.0, 0.0, 0.95]' WHERE id = 1;

-- id=1 moved to z-axis, no longer nearest to [1,0,0]
SELECT 'after_update' AS test, id FROM hnsw_2l ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- id=1 is now nearest to [0,0,1]
SELECT 'moved_to_z' AS test, id FROM hnsw_2l ORDER BY v <-> '[0.0, 0.0, 1.0]' LIMIT 1;

-- ================================================================
-- Test 3: INSERT creates delta-only vector (not in base graph)
-- ================================================================
INSERT INTO hnsw_2l (id, v) VALUES (4, '[0.99, 0.0, 0.0]');

-- New delta-only vector should be nearest to [1,0,0]
SELECT 'delta_insert' AS test, id FROM hnsw_2l ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ================================================================
-- Test 4: DELETE visibility
-- ================================================================
DELETE FROM hnsw_2l WHERE id = 4;

-- id=4 deleted, nearest should be id=2
SELECT 'after_delete' AS test, id FROM hnsw_2l ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- LIMIT 3 should return exactly 3 rows (id=1 updated, not deleted)
SELECT 'count_visible' AS test, count(*)
FROM (SELECT id FROM hnsw_2l ORDER BY v <-> '[0.5, 0.5, 0.5]' LIMIT 10) t;

-- ================================================================
-- Test 5: Multiple sequential UPDATEs, latest wins
-- ================================================================
UPDATE hnsw_2l SET v = '[0.0, 0.95, 0.0]' WHERE id = 3;
UPDATE hnsw_2l SET v = '[0.95, 0.0, 0.0]' WHERE id = 3;

-- id=3 final position is [0.95,0,0], nearest to [1,0,0]
SELECT 'multi_update' AS test, id FROM hnsw_2l ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ================================================================
-- Cleanup (retry: DROP may conflict with HNSW merge lock — #2058)
-- S3 mode merge is slower; wait for it to finish. Keep the correctness
-- assertions above under the normal session settings, but let cleanup wait
-- out the merge fence instead of failing the regression gate on timeout.
-- ================================================================
SET statement_timeout = 0;
SELECT pg_sleep(8);
DROP TABLE IF EXISTS hnsw_2l;

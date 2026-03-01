-- HNSW Batch Maintenance Regression Tests
-- Tests: multi-row INSERT/UPDATE batch graph maintenance,
--        ON CONFLICT interaction, error rollback, UPDATE with vector change.
-- Validates fix for issue #1284 (O(N) → O(1) graph rewrite per statement).

DROP TABLE IF EXISTS hnsw_batch_test;
CREATE TABLE hnsw_batch_test (
    id INT PRIMARY KEY,
    v VECTOR(3)
);
CREATE INDEX idx_hnsw_batch ON hnsw_batch_test USING hnsw (v vector_l2_ops);

-- ============================================================
-- Test 1: Multi-row INSERT (batch graph maintenance)
-- All rows should be visible in a single k-NN search after statement.
-- ============================================================
INSERT INTO hnsw_batch_test (id, v) VALUES
    (1, '[1.0, 0.0, 0.0]'),
    (2, '[0.0, 1.0, 0.0]'),
    (3, '[0.0, 0.0, 1.0]'),
    (4, '[0.5, 0.5, 0.0]');

SELECT 'multi_row_insert' AS test_name,
       (count(*) = 4) AS ok
FROM (
    SELECT id FROM hnsw_batch_test ORDER BY v <-> '[0.5, 0.5, 0.0]' LIMIT 10
) nn;

-- Verify closest to [0.5, 0.5, 0.0] is id=4 (exact match).
SELECT 'insert_nearest' AS test_name,
       ((SELECT id FROM hnsw_batch_test ORDER BY v <-> '[0.5, 0.5, 0.0]' LIMIT 1) = 4) AS ok;

-- ============================================================
-- Test 2: Multi-row UPDATE (batch graph maintenance)
-- Move vectors for id=1 and id=2; search should reflect new positions.
-- ============================================================
UPDATE hnsw_batch_test SET v = '[0.9, 0.9, 0.0]' WHERE id = 1;
UPDATE hnsw_batch_test SET v = '[0.8, 0.8, 0.0]' WHERE id = 2;

-- After updates, nearest to [1, 1, 0] should be id=1 (at [0.9,0.9,0]).
SELECT 'update_nearest' AS test_name,
       ((SELECT id FROM hnsw_batch_test ORDER BY v <-> '[1.0, 1.0, 0.0]' LIMIT 1) = 1) AS ok;

-- ============================================================
-- Test 3: Single-statement multi-row UPDATE (batch deferred path)
-- Update all rows matching WHERE in one statement.
-- ============================================================
UPDATE hnsw_batch_test SET v = v WHERE id IN (1, 2, 3, 4);

-- All 4 rows should still be searchable.
SELECT 'batch_update_all_visible' AS test_name,
       (count(*) = 4) AS ok
FROM (
    SELECT id FROM hnsw_batch_test ORDER BY v <-> '[0.0, 0.0, 0.0]' LIMIT 10
) nn;

-- ============================================================
-- Test 4: INSERT ... ON CONFLICT DO NOTHING with HNSW
-- Conflicting rows should be skipped, non-conflicting inserted.
-- ============================================================
INSERT INTO hnsw_batch_test (id, v) VALUES
    (4, '[9.0, 9.0, 9.0]'),
    (5, '[0.1, 0.1, 0.1]')
ON CONFLICT (id) DO NOTHING;

-- id=5 should be inserted, id=4 should keep its original vector.
SELECT 'on_conflict_do_nothing_new' AS test_name,
       EXISTS (
           SELECT 1 FROM hnsw_batch_test WHERE id = 5
       ) AS ok;

SELECT 'on_conflict_do_nothing_old' AS test_name,
       ((SELECT v FROM hnsw_batch_test WHERE id = 4) != '[9,9,9]'::vector(3)) AS ok;

-- ============================================================
-- Test 5: INSERT ... ON CONFLICT DO UPDATE with HNSW
-- Conflicting row should be updated, new row inserted.
-- ============================================================
INSERT INTO hnsw_batch_test (id, v) VALUES
    (5, '[5.0, 5.0, 5.0]'),
    (6, '[0.0, 0.0, 0.5]')
ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v;

-- id=5 should now have [5,5,5], id=6 should exist.
SELECT 'on_conflict_do_update' AS test_name,
       ((SELECT v FROM hnsw_batch_test WHERE id = 5) = '[5,5,5]'::vector(3)) AS ok;

SELECT 'on_conflict_new_row' AS test_name,
       EXISTS (SELECT 1 FROM hnsw_batch_test WHERE id = 6) AS ok;

-- Nearest to [5,5,5] should be id=5.
SELECT 'on_conflict_search' AS test_name,
       ((SELECT id FROM hnsw_batch_test ORDER BY v <-> '[5.0, 5.0, 5.0]' LIMIT 1) = 5) AS ok;

-- ============================================================
-- Test 6: Transaction rollback undoes batch maintenance
-- ============================================================
BEGIN;
INSERT INTO hnsw_batch_test (id, v) VALUES
    (100, '[10.0, 10.0, 10.0]'),
    (101, '[11.0, 11.0, 11.0]');
ROLLBACK;

SELECT 'rollback_insert' AS test_name,
       (NOT EXISTS (SELECT 1 FROM hnsw_batch_test WHERE id = 100)) AS ok;

BEGIN;
UPDATE hnsw_batch_test SET v = '[99.0, 99.0, 99.0]' WHERE id = 1;
ROLLBACK;

-- id=1 should still have pre-rollback vector.
SELECT 'rollback_update' AS test_name,
       ((SELECT v FROM hnsw_batch_test WHERE id = 1) != '[99,99,99]'::vector(3)) AS ok;

-- ============================================================
-- Test 7: UPDATE setting vector to NULL
-- Verify via direct table scan that the row's vector is NULL.
-- (HNSW scan filtering of stale graph entries is tested separately
-- in 262_hnsw_dml; here we only validate DML correctness.)
-- ============================================================
UPDATE hnsw_batch_test SET v = NULL WHERE id = 6;

-- id=6 should have a NULL vector after the update.
SELECT 'null_vector_set' AS test_name,
       ((SELECT v FROM hnsw_batch_test WHERE id = 6) IS NULL) AS ok;

-- ============================================================
-- Test 8: INSERT...SELECT multi-row with HNSW
-- ============================================================
DROP TABLE IF EXISTS hnsw_batch_source;
CREATE TABLE hnsw_batch_source (
    id INT PRIMARY KEY,
    v VECTOR(3)
);
INSERT INTO hnsw_batch_source (id, v) VALUES
    (200, '[2.0, 0.0, 0.0]'),
    (201, '[0.0, 2.0, 0.0]');

INSERT INTO hnsw_batch_test (id, v)
SELECT id, v FROM hnsw_batch_source;

SELECT 'insert_select_visible' AS test_name,
       ((SELECT count(*) FROM (
           SELECT id FROM hnsw_batch_test ORDER BY v <-> '[2.0, 0.0, 0.0]' LIMIT 20
       ) nn WHERE id IN (200, 201)) = 2) AS ok;

DROP TABLE hnsw_batch_source;

-- ============================================================
-- Cleanup
-- ============================================================
DROP TABLE hnsw_batch_test;

-- HNSW Multi-Index Isolation Regression
-- Purpose:
-- 1) Verify per-column HNSW maintenance correctness when a table has 2 HNSW indexes.
-- 2) Ensure updating v1 does not regress v2 search correctness, and vice versa.

DROP TABLE IF EXISTS hnsw_multi_idx_test;
CREATE TABLE hnsw_multi_idx_test (
    id INT PRIMARY KEY,
    v1 VECTOR(3),
    v2 VECTOR(3)
);

CREATE INDEX idx_hnsw_v1 ON hnsw_multi_idx_test USING hnsw (v1 vector_l2_ops);
CREATE INDEX idx_hnsw_v2 ON hnsw_multi_idx_test USING hnsw (v2 vector_l2_ops);

INSERT INTO hnsw_multi_idx_test (id, v1, v2) VALUES
    (1, '[0.90, 0.00, 0.00]', '[0.80, 0.00, 0.00]'),
    (2, '[0.60, 0.00, 0.00]', '[0.95, 0.00, 0.00]'),
    (3, '[0.00, 0.00, 0.90]', '[0.00, 0.00, 0.90]');

-- Baseline nearest-neighbor checks for each index expression.
SELECT 'baseline_v1' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v1 <-> '[1.0,0.0,0.0]' LIMIT 1) = 1) AS ok;

SELECT 'baseline_v2' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v2 <-> '[1.0,0.0,0.0]' LIMIT 1) = 2) AS ok;

-- Update only v1 on id=1; v1 nearest should move to id=2.
UPDATE hnsw_multi_idx_test
SET v1 = '[0.00, 0.00, 0.95]'
WHERE id = 1;

SELECT 'after_v1_update_v1' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v1 <-> '[1.0,0.0,0.0]' LIMIT 1) = 2) AS ok;

-- v2 path should remain unchanged by v1-only update.
SELECT 'after_v1_update_v2_unchanged' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v2 <-> '[1.0,0.0,0.0]' LIMIT 1) = 2) AS ok;

-- Update only v2 on id=2; v2 nearest should move to id=1.
UPDATE hnsw_multi_idx_test
SET v2 = '[0.00, 0.00, 0.95]'
WHERE id = 2;

SELECT 'after_v2_update_v2' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v2 <-> '[1.0,0.0,0.0]' LIMIT 1) = 1) AS ok;

-- v1 path should remain unchanged by v2-only update.
SELECT 'after_v2_update_v1_unchanged' AS test_name,
       ((SELECT id FROM hnsw_multi_idx_test ORDER BY v1 <-> '[1.0,0.0,0.0]' LIMIT 1) = 2) AS ok;

DROP TABLE hnsw_multi_idx_test;

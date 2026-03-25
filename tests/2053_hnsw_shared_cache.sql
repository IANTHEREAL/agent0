-- PR #2053: HNSW shared index cache correctness
-- Verifies repeated queries return consistent results (shared base graph)
-- and cache eviction works on DROP/TRUNCATE.

-- ================================================================
-- Setup
-- ================================================================
DROP TABLE IF EXISTS hnsw_cache_t;
CREATE TABLE hnsw_cache_t (id INT PRIMARY KEY, v VECTOR(3));
INSERT INTO hnsw_cache_t (id, v) VALUES
    (1, '[1.0, 0.0, 0.0]'),
    (2, '[0.0, 1.0, 0.0]'),
    (3, '[0.0, 0.0, 1.0]'),
    (4, '[0.5, 0.5, 0.0]'),
    (5, '[0.0, 0.5, 0.5]');
CREATE INDEX idx_cache_t ON hnsw_cache_t USING hnsw (v vector_l2_ops);

-- ================================================================
-- Test 1: Repeated identical query (cache hit path)
-- ================================================================
SELECT 'q1' AS tag, id FROM hnsw_cache_t ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;
SELECT 'q2' AS tag, id FROM hnsw_cache_t ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ================================================================
-- Test 2: Different query vector (same cached graph)
-- ================================================================
SELECT 'q3' AS tag, id FROM hnsw_cache_t ORDER BY v <-> '[0.0, 0.0, 1.0]' LIMIT 1;

-- ================================================================
-- Test 3: Consistency check
-- ================================================================
SELECT 'q4' AS tag, id FROM hnsw_cache_t ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

DROP TABLE hnsw_cache_t;

-- ================================================================
-- Test 4: Cache eviction on TRUNCATE
-- ================================================================
DROP TABLE IF EXISTS hnsw_trunc_t;
CREATE TABLE hnsw_trunc_t (id INT PRIMARY KEY, v VECTOR(3));
INSERT INTO hnsw_trunc_t (id, v) VALUES (1, '[1.0, 0.0, 0.0]'), (2, '[0.0, 1.0, 0.0]');
CREATE INDEX idx_trunc_t ON hnsw_trunc_t USING hnsw (v vector_l2_ops);

-- Warm cache
SELECT 'pre_trunc' AS tag, id FROM hnsw_trunc_t ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

TRUNCATE TABLE hnsw_trunc_t;
INSERT INTO hnsw_trunc_t (id, v) VALUES (10, '[0.0, 0.0, 1.0]'), (11, '[0.0, 0.9, 0.0]');

-- Must reflect new data, not stale cache
SELECT 'post_trunc' AS tag, id FROM hnsw_trunc_t ORDER BY v <-> '[0.0, 1.0, 0.0]' LIMIT 1;

DROP TABLE hnsw_trunc_t;

-- ================================================================
-- Test 5: Cache eviction on DROP TABLE + recreate
-- ================================================================
DROP TABLE IF EXISTS hnsw_drop_c;
CREATE TABLE hnsw_drop_c (id INT PRIMARY KEY, v VECTOR(3));
INSERT INTO hnsw_drop_c (id, v) VALUES (1, '[1.0, 0.0, 0.0]'), (2, '[0.0, 1.0, 0.0]');
CREATE INDEX idx_drop_c ON hnsw_drop_c USING hnsw (v vector_l2_ops);

-- Warm cache
SELECT 'phase1' AS tag, id FROM hnsw_drop_c ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

DROP TABLE hnsw_drop_c;

CREATE TABLE hnsw_drop_c (id INT PRIMARY KEY, v VECTOR(3));
INSERT INTO hnsw_drop_c (id, v) VALUES (100, '[0.0, 0.0, 1.0]'), (200, '[0.0, 0.9, 0.0]');
CREATE INDEX idx_drop_c ON hnsw_drop_c USING hnsw (v vector_l2_ops);

-- Must use new index
SELECT 'phase2' AS tag, id FROM hnsw_drop_c ORDER BY v <-> '[0.0, 1.0, 0.0]' LIMIT 1;

DROP TABLE hnsw_drop_c;

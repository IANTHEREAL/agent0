-- HNSW Edge Cases
-- Tests: empty table, k > n, error conditions

-- Empty table with HNSW index
DROP TABLE IF EXISTS hnsw_empty_test;
CREATE TABLE hnsw_empty_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);
CREATE INDEX idx_hnsw_empty ON hnsw_empty_test USING hnsw (v vector_l2_ops);
SELECT id, v FROM hnsw_empty_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 5;
DROP TABLE hnsw_empty_test;

-- k > number of vectors (LIMIT larger than row count)
DROP TABLE IF EXISTS hnsw_k_test;
CREATE TABLE hnsw_k_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);
INSERT INTO hnsw_k_test (v) VALUES ('[1.0, 0.0, 0.0]');
INSERT INTO hnsw_k_test (v) VALUES ('[0.0, 1.0, 0.0]');
CREATE INDEX idx_hnsw_k ON hnsw_k_test USING hnsw (v vector_l2_ops);
SELECT id, v FROM hnsw_k_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 100;
DROP TABLE hnsw_k_test;

-- Error: HNSW index on non-vector column
DROP TABLE IF EXISTS hnsw_error_test;
CREATE TABLE hnsw_error_test (
    id SERIAL PRIMARY KEY,
    name TEXT
);
CREATE INDEX idx_hnsw_err ON hnsw_error_test USING hnsw (name vector_l2_ops);
DROP TABLE hnsw_error_test;

-- Error: multi-column HNSW index
DROP TABLE IF EXISTS hnsw_multi_test;
CREATE TABLE hnsw_multi_test (
    id SERIAL PRIMARY KEY,
    v1 VECTOR(3),
    v2 VECTOR(3)
);
CREATE INDEX idx_hnsw_multi ON hnsw_multi_test USING hnsw (v1, v2);
DROP TABLE hnsw_multi_test;

-- Error: UNIQUE HNSW index (not supported, must error with SQLSTATE 0A000)
DROP TABLE IF EXISTS hnsw_unique_test;
CREATE TABLE hnsw_unique_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);
CREATE UNIQUE INDEX idx_hnsw_uniq ON hnsw_unique_test USING hnsw (v vector_l2_ops);
DROP TABLE hnsw_unique_test;

-- db9 divergence: db9 rejects negative PK for HNSW indexes (PostgreSQL allows it; db9 requires non-negative INTEGER/BIGINT for usearch labels)
-- Error: HNSW index on table with negative PK (must be rejected at DDL time)
DROP TABLE IF EXISTS hnsw_neg_pk_test;
CREATE TABLE hnsw_neg_pk_test (
    id INTEGER PRIMARY KEY,
    v VECTOR(3)
);
INSERT INTO hnsw_neg_pk_test VALUES (-1, '[1.0, 0.0, 0.0]');
INSERT INTO hnsw_neg_pk_test VALUES (1, '[0.0, 1.0, 0.0]');
CREATE INDEX idx_hnsw_neg ON hnsw_neg_pk_test USING hnsw (v vector_l2_ops);
DROP TABLE hnsw_neg_pk_test;

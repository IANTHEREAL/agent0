-- Safety regression tests for unsupported index type rejection (PR #2015).
-- Validates that hash, brin, spgist, gist are explicitly rejected with clear errors.

-- ================================================================
-- Setup
-- ================================================================
CREATE TABLE idx_type_test (id INT PRIMARY KEY, val INT, name TEXT);

-- ================================================================
-- Test 1: HASH index rejected
-- ================================================================
CREATE INDEX idx_hash ON idx_type_test USING hash (val);

-- ================================================================
-- Test 2: BRIN index rejected
-- ================================================================
CREATE INDEX idx_brin ON idx_type_test USING brin (val);

-- ================================================================
-- Test 3: SP-GiST index rejected
-- ================================================================
CREATE INDEX idx_spgist ON idx_type_test USING spgist (name);

-- ================================================================
-- Test 4: GiST index rejected
-- ================================================================
CREATE INDEX idx_gist ON idx_type_test USING gist (name);

-- ================================================================
-- Test 5: Btree (default) succeeds
-- ================================================================
CREATE INDEX idx_btree ON idx_type_test USING btree (val);
SELECT indexname FROM pg_catalog.pg_indexes WHERE tablename = 'idx_type_test' AND indexname = 'idx_btree';

-- ================================================================
-- Test 6: HNSW (supported) succeeds
-- ================================================================
ALTER TABLE idx_type_test ADD COLUMN embedding VECTOR(3);
CREATE INDEX idx_hnsw ON idx_type_test USING hnsw (embedding vector_l2_ops);
SELECT indexname FROM pg_catalog.pg_indexes WHERE tablename = 'idx_type_test' AND indexname = 'idx_hnsw';

-- ================================================================
-- Cleanup
-- ================================================================
DROP TABLE idx_type_test;

-- HNSW Basic Index Operations
-- Tests: CREATE INDEX, basic k-NN search, EXPLAIN, DROP INDEX

DROP TABLE IF EXISTS hnsw_basic_test;

CREATE TABLE hnsw_basic_test (
    id SERIAL PRIMARY KEY,
    embedding VECTOR(3)
);

-- Insert test vectors with known distances
INSERT INTO hnsw_basic_test (embedding) VALUES ('[1.0, 0.0, 0.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[0.0, 1.0, 0.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[0.0, 0.0, 1.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[1.0, 1.0, 0.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[1.0, 1.0, 1.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[0.5, 0.5, 0.5]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[2.0, 0.0, 0.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[0.0, 2.0, 0.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[0.0, 0.0, 2.0]');
INSERT INTO hnsw_basic_test (embedding) VALUES ('[3.0, 3.0, 3.0]');

-- Create HNSW index with L2 distance
CREATE INDEX idx_hnsw_l2 ON hnsw_basic_test USING hnsw (embedding vector_l2_ops);

-- Basic k-NN search: find 3 nearest to [1,0,0]
SELECT id, embedding FROM hnsw_basic_test ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 3;

-- EXPLAIN should show HNSW Scan
EXPLAIN SELECT id FROM hnsw_basic_test ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 5;

-- Drop index
DROP INDEX idx_hnsw_l2;

-- Cleanup
DROP TABLE hnsw_basic_test;

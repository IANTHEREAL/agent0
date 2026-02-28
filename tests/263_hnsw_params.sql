-- HNSW Parameters
-- Tests: WITH clause (m, ef_construction), ef_search GUC

DROP TABLE IF EXISTS hnsw_params_test;

CREATE TABLE hnsw_params_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);

INSERT INTO hnsw_params_test (v) VALUES ('[1.0, 0.0, 0.0]');
INSERT INTO hnsw_params_test (v) VALUES ('[0.0, 1.0, 0.0]');
INSERT INTO hnsw_params_test (v) VALUES ('[0.0, 0.0, 1.0]');

-- Create with custom parameters
CREATE INDEX idx_hnsw_custom ON hnsw_params_test USING hnsw (v vector_l2_ops) WITH (m = 32, ef_construction = 128);

-- Test ef_search GUC
SHOW hnsw.ef_search;
SET hnsw.ef_search = 100;
SHOW hnsw.ef_search;

-- Search should still work with different ef_search
SELECT id, v FROM hnsw_params_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 2;

-- Reset to default
SET hnsw.ef_search = 40;
SHOW hnsw.ef_search;

-- Cleanup
DROP TABLE hnsw_params_test;

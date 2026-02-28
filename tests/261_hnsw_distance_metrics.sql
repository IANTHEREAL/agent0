-- HNSW Distance Metrics
-- Tests: L2, cosine, inner product with separate indexes

DROP TABLE IF EXISTS hnsw_metrics_test;

CREATE TABLE hnsw_metrics_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);

INSERT INTO hnsw_metrics_test (v) VALUES ('[1.0, 0.0, 0.0]');
INSERT INTO hnsw_metrics_test (v) VALUES ('[0.0, 1.0, 0.0]');
INSERT INTO hnsw_metrics_test (v) VALUES ('[0.0, 0.0, 1.0]');
INSERT INTO hnsw_metrics_test (v) VALUES ('[0.7, 0.7, 0.0]');
INSERT INTO hnsw_metrics_test (v) VALUES ('[1.0, 1.0, 1.0]');

-- Test L2 distance index
CREATE INDEX idx_l2 ON hnsw_metrics_test USING hnsw (v vector_l2_ops);
SELECT id, v FROM hnsw_metrics_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 3;
DROP INDEX idx_l2;

-- Test cosine distance index
CREATE INDEX idx_cos ON hnsw_metrics_test USING hnsw (v vector_cosine_ops);
SELECT id, v FROM hnsw_metrics_test ORDER BY v <=> '[1.0, 0.0, 0.0]' LIMIT 3;
DROP INDEX idx_cos;

-- Test inner product index
CREATE INDEX idx_ip ON hnsw_metrics_test USING hnsw (v vector_ip_ops);
SELECT id, v FROM hnsw_metrics_test ORDER BY v <#> '[1.0, 0.0, 0.0]' LIMIT 3;
DROP INDEX idx_ip;

-- Cleanup
DROP TABLE hnsw_metrics_test;

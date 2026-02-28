-- HNSW DML Maintenance
-- Tests: INSERT after index creation, DELETE effects, NULL vectors

DROP TABLE IF EXISTS hnsw_dml_test;

CREATE TABLE hnsw_dml_test (
    id SERIAL PRIMARY KEY,
    v VECTOR(3)
);

-- Insert initial data
INSERT INTO hnsw_dml_test (v) VALUES ('[1.0, 0.0, 0.0]');
INSERT INTO hnsw_dml_test (v) VALUES ('[0.0, 1.0, 0.0]');
INSERT INTO hnsw_dml_test (v) VALUES ('[0.0, 0.0, 1.0]');

-- Create index on existing data
CREATE INDEX idx_hnsw_dml ON hnsw_dml_test USING hnsw (v vector_l2_ops);

-- INSERT after index creation — new vector should appear in search
INSERT INTO hnsw_dml_test (v) VALUES ('[0.9, 0.1, 0.0]');
SELECT id, v FROM hnsw_dml_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 2;

-- INSERT NULL vector — should not crash
INSERT INTO hnsw_dml_test (v) VALUES (NULL);
SELECT id, v FROM hnsw_dml_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 3;

-- DELETE — deleted vector should not appear
DELETE FROM hnsw_dml_test WHERE id = 1;
SELECT id, v FROM hnsw_dml_test ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 3;

-- Cleanup
DROP TABLE hnsw_dml_test;

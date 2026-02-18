-- CREATE INDEX CONCURRENTLY test
-- Verifies syntax acceptance and index creation

DROP TABLE IF EXISTS cic_test;
CREATE TABLE cic_test (id INTEGER PRIMARY KEY, val TEXT);
INSERT INTO cic_test VALUES (1, 'a'), (2, 'b'), (3, 'c');

-- Regular CREATE INDEX should work
CREATE INDEX idx_cic_val ON cic_test(val);

-- Verify index is usable
SELECT val FROM cic_test WHERE val = 'b';

-- CREATE INDEX CONCURRENTLY should succeed (enqueues background task)
CREATE INDEX CONCURRENTLY idx_cic_val2 ON cic_test(id);

-- Clean up
DROP TABLE cic_test;

-- REFRESH MATERIALIZED VIEW CONCURRENTLY test
-- Tests both synchronous and concurrent refresh paths

DROP MATERIALIZED VIEW IF EXISTS mv_test;
DROP TABLE IF EXISTS mv_source;
CREATE TABLE mv_source (id INTEGER PRIMARY KEY, val TEXT);
INSERT INTO mv_source VALUES (1, 'hello'), (2, 'world');

-- Create materialized view
CREATE MATERIALIZED VIEW mv_test AS SELECT * FROM mv_source ORDER BY id;
SELECT * FROM mv_test ORDER BY id;

-- REFRESH CONCURRENTLY (enqueues in background)
REFRESH MATERIALIZED VIEW CONCURRENTLY mv_test;

-- Regular REFRESH should still work synchronously
INSERT INTO mv_source VALUES (3, 'test');
REFRESH MATERIALIZED VIEW mv_test;
SELECT * FROM mv_test ORDER BY id;

-- Clean up
DROP MATERIALIZED VIEW mv_test;
DROP TABLE mv_source;

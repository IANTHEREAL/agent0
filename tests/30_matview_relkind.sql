-- pg_class.relkind for materialized views should be 'm', not 'r'.
-- Regression test for #1658.

DROP MATERIALIZED VIEW IF EXISTS mv_relkind_test;
DROP VIEW IF EXISTS v_relkind_test;
DROP TABLE IF EXISTS t_relkind_test;

CREATE TABLE t_relkind_test (id INT PRIMARY KEY, val TEXT);
CREATE VIEW v_relkind_test AS SELECT id, val FROM t_relkind_test;
CREATE MATERIALIZED VIEW mv_relkind_test AS SELECT id, val FROM t_relkind_test;

-- Verify relkind values: 'r' for table, 'v' for view, 'm' for matview
SELECT relname, relkind
FROM pg_catalog.pg_class
WHERE relname IN ('t_relkind_test', 'v_relkind_test', 'mv_relkind_test')
ORDER BY relname;

-- Cleanup
DROP MATERIALIZED VIEW mv_relkind_test;
DROP VIEW v_relkind_test;
DROP TABLE t_relkind_test;

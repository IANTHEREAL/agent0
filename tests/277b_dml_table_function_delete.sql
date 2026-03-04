-- DB9_DIVERGENCE(#1247): db9-specific guard on DML auxiliary table-function materialization.
-- PostgreSQL 17.7 validation (2026-03-04):
--   - DELETE with generate_series succeeds (`DELETE 3`).
-- db9-specific: db9 guards DML auxiliary table-function materialization and errors.

DROP TABLE IF EXISTS tf_target;
CREATE TABLE tf_target (id INT PRIMARY KEY, val TEXT);
INSERT INTO tf_target VALUES (1, 'a'), (2, 'b'), (3, 'c');

SET db9.dml_table_scan_max_rows = 5;

-- Non-streaming table function path (generate_series) must be guarded.
DELETE FROM tf_target
USING (
  SELECT generate_series AS id
  FROM generate_series(1, 100)
) s
WHERE tf_target.id = s.id;

DROP TABLE tf_target;

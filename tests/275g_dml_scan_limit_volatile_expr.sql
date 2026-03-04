-- DB9_DIVERGENCE(#1247): db9-specific DML auxiliary source row guard and error behavior.
-- PostgreSQL 17.7 validation (2026-03-04):
--   - `SET db9.dml_table_scan_max_rows = 3` is accepted as a placeholder GUC.
--   - Volatile LIMIT expression executes successfully in PG (UPDATE 5).
-- db9-specific: db9 enforces DML auxiliary row guard and raises an error instead.

DROP TABLE IF EXISTS dml_err_t1;
DROP TABLE IF EXISTS dml_err_t2;
CREATE TABLE dml_err_t1 (id INT PRIMARY KEY, val TEXT);
CREATE TABLE dml_err_t2 (id INT PRIMARY KEY, val TEXT);
INSERT INTO dml_err_t1 VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e');
INSERT INTO dml_err_t2 VALUES (1, 'x'), (2, 'y'), (3, 'z'), (4, 'w'), (5, 'v');

SET db9.dml_table_scan_max_rows = 3;

-- Volatile/non-constant LIMIT expression still fails under DML guard.
UPDATE dml_err_t1
SET val = s.val
FROM (
  SELECT id, val
  FROM dml_err_t2
  LIMIT (CASE WHEN pg_backend_pid() > 0 THEN 5 ELSE 0 END)
) s
WHERE dml_err_t1.id = s.id;

DROP TABLE dml_err_t1;
DROP TABLE dml_err_t2;

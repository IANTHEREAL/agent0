-- DB9_DIVERGENCE(#1247): db9-specific DML auxiliary source row guard and error behavior.
-- PostgreSQL 17.7 validation (2026-03-04):
--   - `SET db9.dml_table_scan_max_rows = 3` is accepted as a placeholder GUC.
--   - LIMIT NULL is treated as LIMIT ALL in PG (UPDATE 5).
-- db9-specific: db9 rejects LIMIT NULL in DML auxiliary subqueries.

DROP TABLE IF EXISTS dml_err_t1;
DROP TABLE IF EXISTS dml_err_t2;
CREATE TABLE dml_err_t1 (id INT PRIMARY KEY, val TEXT);
CREATE TABLE dml_err_t2 (id INT PRIMARY KEY, val TEXT);
INSERT INTO dml_err_t1 VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e');
INSERT INTO dml_err_t2 VALUES (1, 'x'), (2, 'y'), (3, 'z'), (4, 'w'), (5, 'v');

SET db9.dml_table_scan_max_rows = 3;

-- db9-specific: LIMIT NULL is rejected (PG treats LIMIT NULL as LIMIT ALL).
UPDATE dml_err_t1 SET val = s.val FROM (SELECT id, val FROM dml_err_t2 LIMIT NULL) s WHERE dml_err_t1.id = s.id;

DROP TABLE dml_err_t1;
DROP TABLE dml_err_t2;

-- DB9_DIVERGENCE(#1247): db9-specific DML auxiliary source row guard and GUC behavior.
-- PostgreSQL 17.7 validation (2026-03-04):
--   - `SHOW db9.dml_table_scan_max_rows` before SET -> ERROR: unrecognized parameter.
--   - After `SET db9.dml_table_scan_max_rows = 50`, `SHOW` returns 50.
--   - After `RESET db9.dml_table_scan_max_rows`, `SHOW` returns empty value.
-- db9-specific: db9 exposes this GUC with concrete default/readback semantics.
-- DML auxiliary source row guard: success cases.

-- 1) Default value.
SHOW db9.dml_table_scan_max_rows;

-- 2) SET + readback.
SET db9.dml_table_scan_max_rows = 50;
SHOW db9.dml_table_scan_max_rows;

-- 3) RESET + readback.
RESET db9.dml_table_scan_max_rows;
SHOW db9.dml_table_scan_max_rows;

-- 4) Under-limit UPDATE FROM succeeds.
DROP TABLE IF EXISTS dml_limit_t1;
DROP TABLE IF EXISTS dml_limit_t2;
CREATE TABLE dml_limit_t1 (id INT PRIMARY KEY, val TEXT);
CREATE TABLE dml_limit_t2 (id INT PRIMARY KEY, val TEXT);
INSERT INTO dml_limit_t1 VALUES (1, 'a'), (2, 'b'), (3, 'c');
INSERT INTO dml_limit_t2 VALUES (1, 'x'), (2, 'y'), (3, 'z');
SET db9.dml_table_scan_max_rows = 5;
UPDATE dml_limit_t1
SET val = dml_limit_t2.val
FROM dml_limit_t2
WHERE dml_limit_t1.id = dml_limit_t2.id;
SELECT id, val FROM dml_limit_t1 ORDER BY id;

-- 5) Under-limit DELETE USING succeeds.
INSERT INTO dml_limit_t1 VALUES (4, 'keep');
DELETE FROM dml_limit_t1 USING dml_limit_t2 WHERE dml_limit_t1.id = dml_limit_t2.id;
SELECT id, val FROM dml_limit_t1 ORDER BY id;

-- 6) Guard disabled (0 = unlimited).
INSERT INTO dml_limit_t1 VALUES (1, 'a'), (2, 'b'), (3, 'c');
SET db9.dml_table_scan_max_rows = 0;
UPDATE dml_limit_t1
SET val = dml_limit_t2.val
FROM dml_limit_t2
WHERE dml_limit_t1.id = dml_limit_t2.id;
SELECT id, val FROM dml_limit_t1 ORDER BY id;

-- 7) Dynamic LIMIT under cap still succeeds.
SET db9.dml_table_scan_max_rows = 5;
PREPARE dml_limit_p1(int) AS
UPDATE dml_limit_t1
SET val = s.val
FROM (SELECT id, val FROM dml_limit_t2 LIMIT $1) s
WHERE dml_limit_t1.id = s.id;
EXECUTE dml_limit_p1(3);
SELECT id, val FROM dml_limit_t1 ORDER BY id;
DEALLOCATE dml_limit_p1;

DROP TABLE dml_limit_t1;
DROP TABLE dml_limit_t2;

-- DB9_DIVERGENCE(#1247): db9-specific cross-product guard for DML auxiliary sources.
-- PostgreSQL 17.7 validation (2026-03-04):
--   - `DELETE ... USING cp_aux_a, cp_aux_b` executes and deletes 1 row.
-- db9-specific: db9 adds a combined auxiliary cross-product cap and errors here.
-- Note: DELETE path trips the explicit cross-product materialization guard.

DROP TABLE IF EXISTS cp_target;
DROP TABLE IF EXISTS cp_aux_a;
DROP TABLE IF EXISTS cp_aux_b;

CREATE TABLE cp_target (id INT PRIMARY KEY, val TEXT);
CREATE TABLE cp_aux_a  (id INT PRIMARY KEY, val TEXT);
CREATE TABLE cp_aux_b  (id INT PRIMARY KEY, val TEXT);

INSERT INTO cp_target VALUES (1, 'x');
-- 4 rows each: each source under limit 5, but cross-product is 16.
INSERT INTO cp_aux_a VALUES (1, 'a1'), (2, 'a2'), (3, 'a3'), (4, 'a4');
INSERT INTO cp_aux_b VALUES (1, 'b1'), (2, 'b2'), (3, 'b3'), (4, 'b4');

SET db9.dml_table_scan_max_rows = 5;

-- DELETE USING with two auxiliary sources.
DELETE FROM cp_target
USING cp_aux_a, cp_aux_b
WHERE cp_target.id = cp_aux_a.id;

DROP TABLE cp_target;
DROP TABLE cp_aux_a;
DROP TABLE cp_aux_b;

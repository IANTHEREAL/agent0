-- WITH ... DML statement classification regression test
-- Ref: #2300 — WITH+INSERT/UPDATE/DELETE was misrouted through the query
-- analysis path, producing internal error "query body type: Discriminant(5)".
--
-- PostgreSQL treats each DML as a top-level statement that owns its CTE.
-- After the fix, WITH+DML is parsed as Statement::Insert/Update/Delete
-- with a `with` field, and routed through the DML execution path.
--
-- All expected outputs verified against PostgreSQL 17.9.

-- ── Setup ──────────────────────────────────────────────────────────────
DROP TABLE IF EXISTS wdml_target CASCADE;
DROP TABLE IF EXISTS wdml_source CASCADE;

CREATE TABLE wdml_source (id INT PRIMARY KEY, val TEXT);
INSERT INTO wdml_source VALUES (1, 'a'), (2, 'b'), (3, 'c');

CREATE TABLE wdml_target (id INT PRIMARY KEY, val TEXT);

-- ── 1: WITH ... INSERT ... SELECT ... RETURNING ────────────────────────
WITH src AS (SELECT id, val FROM wdml_source WHERE id <= 2)
INSERT INTO wdml_target SELECT * FROM src RETURNING *;

-- ── 2: Verify insert ──────────────────────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 3: WITH ... UPDATE via WHERE subquery ─────────────────────────────
WITH new_val AS (SELECT upper(val) AS val FROM wdml_source WHERE id = 1)
UPDATE wdml_target SET val = (SELECT val FROM new_val) WHERE id = 1 RETURNING *;

-- ── 4: Verify update ─────────────────────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 5: WITH ... DELETE ... WHERE IN (SELECT ...) RETURNING ─────────────
WITH to_delete AS (SELECT id FROM wdml_source WHERE id = 2)
DELETE FROM wdml_target WHERE id IN (SELECT id FROM to_delete) RETURNING *;

-- ── 6: Verify delete ──────────────────────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 7: WITH RECURSIVE ... INSERT ──────────────────────────────────────
DROP TABLE IF EXISTS wdml_target;
CREATE TABLE wdml_target (id INT PRIMARY KEY, val TEXT);
WITH RECURSIVE nums AS (
    SELECT 1 AS n
    UNION ALL
    SELECT n + 1 FROM nums WHERE n < 3
)
INSERT INTO wdml_target SELECT n, 'row' || n FROM nums RETURNING *;

-- ── 8: Verify recursive insert ────────────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 9: Multiple CTEs chained in INSERT ────────────────────────────────
DROP TABLE IF EXISTS wdml_target;
CREATE TABLE wdml_target (id INT PRIMARY KEY, val TEXT);
WITH
    first_two AS (SELECT id, val FROM wdml_source WHERE id <= 2),
    uppered AS (SELECT id, upper(val) AS val FROM first_two)
INSERT INTO wdml_target SELECT * FROM uppered RETURNING *;

-- ── 10: Verify multiple CTE insert ───────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 11: WITH ... UPDATE (CTE in WHERE subquery) ──────────────────────
WITH threshold AS (SELECT 1 AS min_id)
UPDATE wdml_target SET val = 'updated' WHERE id >= (SELECT min_id FROM threshold) RETURNING *;

-- ── 12: Verify update via WHERE subquery ──────────────────────────────
SELECT * FROM wdml_target ORDER BY id;

-- ── 13: WITH ... DELETE all matching ──────────────────────────────────
WITH all_ids AS (SELECT id FROM wdml_target)
DELETE FROM wdml_target WHERE id IN (SELECT id FROM all_ids) RETURNING *;

-- ── 14: Verify empty table ────────────────────────────────────────────
SELECT count(*) FROM wdml_target;

-- ── 15: CTE name shadows DML target (PG: updates base table) ──────────
DROP TABLE IF EXISTS wdml_target;
CREATE TABLE wdml_target (id INT PRIMARY KEY, val TEXT);
INSERT INTO wdml_target VALUES (1, 'original');
WITH wdml_target AS (SELECT 1 AS id, 'cte_val' AS val)
UPDATE wdml_target SET val = 'updated' WHERE id = 1;
SELECT * FROM wdml_target ORDER BY id;

-- ── 16: PREPARE/EXECUTE with WITH ... INSERT ──────────────────────────
DROP TABLE IF EXISTS wdml_target;
CREATE TABLE wdml_target (id INT PRIMARY KEY, val TEXT);
PREPARE with_ins AS WITH src AS (SELECT id, val FROM wdml_source WHERE id <= 2) INSERT INTO wdml_target SELECT * FROM src;
EXECUTE with_ins;

-- ── 16: Verify prepared insert ────────────────────────────────────────
SELECT * FROM wdml_target ORDER BY id;
DEALLOCATE with_ins;

-- ── Cleanup ────────────────────────────────────────────────────────────
DROP TABLE IF EXISTS wdml_target;
DROP TABLE IF EXISTS wdml_source;

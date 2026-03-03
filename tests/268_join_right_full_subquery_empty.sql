-- Regression: RIGHT/FULL JOIN empty-side fallback must use correct width for
-- subquery/derived inputs (no ctid) vs base table inputs (ctid appended).

-- ── Setup ──────────────────────────────────────────────────────────
DROP TABLE IF EXISTS jrs_t;
CREATE TABLE jrs_t(id INT PRIMARY KEY, val TEXT);
INSERT INTO jrs_t VALUES (1, 'a'), (2, 'b');

-- ── RIGHT JOIN: empty subquery on LEFT side ────────────────────────
-- The left derived table returns 0 rows. Unmatched right rows must be
-- NULL-padded to the subquery width (2 columns, no ctid), not width+1.
SELECT sub.x, sub.y, jrs_t.id, jrs_t.val
FROM (SELECT id AS x, val AS y FROM jrs_t WHERE false) sub
RIGHT JOIN jrs_t ON sub.x = jrs_t.id
ORDER BY jrs_t.id;

-- ── RIGHT JOIN: empty base table on LEFT side (baseline) ──────────
-- Contrasts with above: the left base table returns 0 rows but gets
-- ctid appended, so empty-side width = schema.columns.len() + 1.
TRUNCATE TABLE jrs_t;

SELECT jrs_t.id, jrs_t.val, r.id AS r_id, r.val AS r_val
FROM jrs_t
RIGHT JOIN (SELECT 10 AS id, 'x' AS val) r ON jrs_t.id = r.id;

-- Restore data for FULL JOIN tests.
INSERT INTO jrs_t VALUES (1, 'a'), (2, 'b');

-- ── FULL JOIN: empty subquery on LEFT side ─────────────────────────
SELECT sub.x, sub.y, jrs_t.id, jrs_t.val
FROM (SELECT id AS x, val AS y FROM jrs_t WHERE false) sub
FULL JOIN jrs_t ON sub.x = jrs_t.id
ORDER BY COALESCE(jrs_t.id, sub.x);

-- ── FULL JOIN: empty subquery on RIGHT side ────────────────────────
SELECT jrs_t.id, jrs_t.val, sub.x, sub.y
FROM jrs_t
FULL JOIN (SELECT id AS x, val AS y FROM jrs_t WHERE false) sub ON jrs_t.id = sub.x
ORDER BY COALESCE(jrs_t.id, sub.x);

-- ── FULL JOIN: both sides subqueries, one empty ────────────────────
SELECT a.i, b.j
FROM (SELECT 1 AS i UNION ALL SELECT 2) a
FULL JOIN (SELECT 1 AS j WHERE false) b ON a.i = b.j
ORDER BY COALESCE(a.i, b.j);

-- ── Cleanup ────────────────────────────────────────────────────────
DROP TABLE jrs_t;

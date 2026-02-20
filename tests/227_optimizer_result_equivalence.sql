-- Test: tipg.use_optimizer compatibility GUC is accepted as a no-op.
-- The engine is single-path (optimizer always on); toggles below validate
-- compatibility plumbing/readback behavior without implying dual execution.

DROP TABLE IF EXISTS ore_test;
CREATE TABLE ore_test (
    id INT PRIMARY KEY,
    category TEXT,
    value INT
);

INSERT INTO ore_test VALUES (1, 'A', 10);
INSERT INTO ore_test VALUES (2, 'B', 20);
INSERT INTO ore_test VALUES (3, 'A', 30);
INSERT INTO ore_test VALUES (4, 'B', 40);
INSERT INTO ore_test VALUES (5, 'C', 50);
INSERT INTO ore_test VALUES (6, 'A', 10);

ANALYZE ore_test;

-- ─── Aggregation ─────────────────────────────────────

-- Test 1: GROUP BY + COUNT (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
SELECT category, COUNT(*) AS cnt FROM ore_test GROUP BY category ORDER BY category;

-- Test 2: GROUP BY + COUNT (compat toggle ON)
SET tipg.use_optimizer = on;
SELECT category, COUNT(*) AS cnt FROM ore_test GROUP BY category ORDER BY category;

-- Test 3: GROUP BY + SUM (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
SELECT category, SUM(value) AS total FROM ore_test GROUP BY category ORDER BY category;

-- Test 4: GROUP BY + SUM (compat toggle ON)
SET tipg.use_optimizer = on;
SELECT category, SUM(value) AS total FROM ore_test GROUP BY category ORDER BY category;

-- ─── DISTINCT ────────────────────────────────────────

-- Test 5: DISTINCT (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
SELECT DISTINCT category FROM ore_test ORDER BY category;

-- Test 6: DISTINCT (compat toggle ON)
SET tipg.use_optimizer = on;
SELECT DISTINCT category FROM ore_test ORDER BY category;

-- ─── ORDER BY + LIMIT ────────────────────────────────

-- Test 7: ORDER BY + LIMIT (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
SELECT * FROM ore_test ORDER BY value DESC LIMIT 3;

-- Test 8: ORDER BY + LIMIT (compat toggle ON)
SET tipg.use_optimizer = on;
SELECT * FROM ore_test ORDER BY value DESC LIMIT 3;

-- ─── CTE ─────────────────────────────────────────────

-- Test 9: CTE (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
WITH top_cats AS (
    SELECT category, SUM(value) AS total FROM ore_test GROUP BY category
)
SELECT * FROM top_cats ORDER BY category;

-- Test 10: CTE (compat toggle ON)
SET tipg.use_optimizer = on;
WITH top_cats AS (
    SELECT category, SUM(value) AS total FROM ore_test GROUP BY category
)
SELECT * FROM top_cats ORDER BY category;

-- ─── Subquery in WHERE ───────────────────────────────

-- Test 11: Subquery (compat toggle OFF => no-op)
SET tipg.use_optimizer = off;
SELECT * FROM ore_test WHERE value > (SELECT AVG(value) FROM ore_test) ORDER BY id;

-- Test 12: Subquery (compat toggle ON)
SET tipg.use_optimizer = on;
SELECT * FROM ore_test WHERE value > (SELECT AVG(value) FROM ore_test) ORDER BY id;

DROP TABLE ore_test;

-- Optimizer predicate pushdown: compatibility-GUC no-op checks.
-- `tipg.use_optimizer` toggles are accepted for compatibility; execution stays single-path.
-- All queries use ORDER BY for deterministic output.

CREATE TABLE opt_a (id INT PRIMARY KEY, x INT);
CREATE TABLE opt_b (id INT PRIMARY KEY, aid INT, y INT);
INSERT INTO opt_a VALUES (1, 10), (2, 20), (3, 30);
INSERT INTO opt_b VALUES (1, 1, 100), (2, 2, 200), (3, 9, 300);

-- Test 1: INNER JOIN with left-only WHERE
SET tipg.use_optimizer = off;
SELECT a.id, a.x, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10 ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, a.x, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10 ORDER BY a.id;

-- Test 2: LEFT JOIN with left-only WHERE (safe to push)
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE a.x > 10 ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE a.x > 10 ORDER BY a.id;

-- Test 3: LEFT JOIN with right-side WHERE (must NOT push — nullable side)
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE b.y > 100 ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE b.y > 100 ORDER BY a.id;

-- Test 4: Cross join with filter
SET tipg.use_optimizer = off;
SELECT a.id, b.id FROM opt_a a, opt_b b WHERE a.x = 10 ORDER BY a.id, b.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.id FROM opt_a a, opt_b b WHERE a.x = 10 ORDER BY a.id, b.id;

-- Test 5: INNER JOIN with mixed WHERE (split push)
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10 AND b.y > 50 ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10 AND b.y > 50 ORDER BY a.id;

-- Test 6: Comma-join with equi WHERE
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid ORDER BY a.id;

-- Test 7: Comma-join with equi + single-table filter
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid AND a.x = 10 ORDER BY a.id;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid AND a.x = 10 ORDER BY a.id;

-- Test 8: Comma-join non-equi
SET tipg.use_optimizer = off;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.x > b.y ORDER BY a.id, b.y;
SET tipg.use_optimizer = on;
SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.x > b.y ORDER BY a.id, b.y;

DROP TABLE opt_b;
DROP TABLE opt_a;

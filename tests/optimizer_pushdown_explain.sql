-- Optimizer predicate pushdown: EXPLAIN plan-shape assertions.
-- Validates that predicates are pushed to the correct positions.

CREATE TABLE opt_a (id INT PRIMARY KEY, x INT);
CREATE TABLE opt_b (id INT PRIMARY KEY, aid INT, y INT);
INSERT INTO opt_a VALUES (1, 10), (2, 20), (3, 30);
INSERT INTO opt_b VALUES (1, 1, 100), (2, 2, 200), (3, 9, 300);
SET db9.use_optimizer = on;

-- E1: INNER JOIN + left-only WHERE → Filter pushed below Join
EXPLAIN SELECT a.id, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10;

-- E2: LEFT JOIN + left-only WHERE → Filter pushed to preserved (left) side
EXPLAIN SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE a.x > 10;

-- E3: LEFT JOIN + right-side WHERE → Filter stays ABOVE Join
EXPLAIN SELECT a.id, b.y FROM opt_a a LEFT JOIN opt_b b ON a.id = b.aid WHERE b.y > 100;

-- E4: INNER JOIN + mixed WHERE → split push to both sides
EXPLAIN SELECT a.id, b.y FROM opt_a a JOIN opt_b b ON a.id = b.aid WHERE a.x = 10 AND b.y > 50;

-- E5: Comma-join with equi WHERE → should show Hash Join (cross-join elimination)
EXPLAIN SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid;

-- E6: Comma-join with equi WHERE + single-table filter
EXPLAIN SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.id = b.aid AND a.x = 10;

-- E7: Comma-join with non-equi WHERE → stays as Nested Loop
EXPLAIN SELECT a.id, b.y FROM opt_a a, opt_b b WHERE a.x > b.y;

DROP TABLE opt_b;
DROP TABLE opt_a;

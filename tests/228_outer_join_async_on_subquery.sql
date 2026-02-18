-- Regression test for f8f8852: correlated subqueries in outer JOIN ON conditions.
-- After moving async JOIN ON evaluation into operators, null-extension semantics
-- must be preserved for LEFT/RIGHT/FULL joins with subquery-containing ON clauses.
--
-- Coverage gap: existing tests cover correlated subqueries in INNER JOIN ON
-- (tests 155-158) and outer joins with USING/NATURAL (tests 107, 115, 116),
-- but no test combined LEFT/RIGHT/FULL JOIN ... ON <subquery>.

DROP TABLE IF EXISTS ojsub_a;
DROP TABLE IF EXISTS ojsub_b;
DROP TABLE IF EXISTS ojsub_map;

CREATE TABLE ojsub_a(id INT, val TEXT);
CREATE TABLE ojsub_b(id INT, val TEXT);
CREATE TABLE ojsub_map(a_id INT, b_id INT);

INSERT INTO ojsub_a VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');
INSERT INTO ojsub_b VALUES (10, 'ten'), (20, 'twenty'), (30, 'thirty');
-- map: a.id=1 -> b.id=10, a.id=2 -> b.id=20; a.id=3 has NO mapping
INSERT INTO ojsub_map VALUES (1, 10), (2, 20);

-- 1) LEFT JOIN with scalar subquery in ON.
--    a.id=3 has no mapping -> must be null-extended, not dropped.
SELECT a.id, a.val, b.id AS b_id, b.val AS b_val
FROM ojsub_a a
LEFT JOIN ojsub_b b ON b.id = (SELECT m.b_id FROM ojsub_map m WHERE m.a_id = a.id)
ORDER BY a.id;

-- 2) RIGHT JOIN with scalar subquery in ON.
--    b.id=30 has no mapping -> must be null-extended.
SELECT a.id AS a_id, a.val AS a_val, b.id, b.val
FROM ojsub_a a
RIGHT JOIN ojsub_b b ON a.id = (SELECT m.a_id FROM ojsub_map m WHERE m.b_id = b.id)
ORDER BY b.id;

-- 3) FULL JOIN with scalar subquery in ON.
--    a.id=3 unmatched (null-extend right), b.id=30 unmatched (null-extend left).
SELECT a.id AS a_id, a.val AS a_val, b.id AS b_id, b.val AS b_val
FROM ojsub_a a
FULL JOIN ojsub_b b ON b.id = (SELECT m.b_id FROM ojsub_map m WHERE m.a_id = a.id)
ORDER BY COALESCE(a.id, 0), COALESCE(b.id, 0);

-- 4) LEFT JOIN + scalar subquery ON + GROUP BY.
--    Previously rejected ("correlated subqueries in outer join ON conditions
--    with GROUP BY/DISTINCT are not yet supported"); now handled in-operator.
SELECT a.val, COUNT(b.id) AS match_count
FROM ojsub_a a
LEFT JOIN ojsub_b b ON b.id = (SELECT m.b_id FROM ojsub_map m WHERE m.a_id = a.id)
GROUP BY a.val
ORDER BY a.val;

-- 5) LEFT JOIN with EXISTS subquery in ON.
--    Tests a different subquery kind (EXISTS vs scalar) in outer join ON.
SELECT a.id, a.val, b.id AS b_id, b.val AS b_val
FROM ojsub_a a
LEFT JOIN ojsub_b b ON EXISTS (SELECT 1 FROM ojsub_map m WHERE m.a_id = a.id AND m.b_id = b.id)
ORDER BY a.id, b.id;

-- 6) FULL JOIN with EXISTS subquery in ON.
--    Both unmatched sides must be null-extended.
SELECT a.id AS a_id, a.val AS a_val, b.id AS b_id, b.val AS b_val
FROM ojsub_a a
FULL JOIN ojsub_b b ON EXISTS (SELECT 1 FROM ojsub_map m WHERE m.a_id = a.id AND m.b_id = b.id)
ORDER BY COALESCE(a.id, 0), COALESCE(b.id, 0);

DROP TABLE ojsub_a;
DROP TABLE ojsub_b;
DROP TABLE ojsub_map;

-- Issue #420 regression:
-- Correlated scalar subqueries in JOIN ON conditions must remain correlated, and internal
-- `__tipg_subquery_*` computed columns must not leak into `SELECT *` output with USING/NATURAL joins.

DROP TABLE IF EXISTS i420_a;
DROP TABLE IF EXISTS i420_b;
DROP TABLE IF EXISTS i420_d;
DROP TABLE IF EXISTS i420_e;

CREATE TABLE i420_a(id INT, a_val INT);
CREATE TABLE i420_b(id INT, b_val INT);
CREATE TABLE i420_d(v INT);
CREATE TABLE i420_e(id INT, v INT);

INSERT INTO i420_a VALUES (1,10),(2,20);
INSERT INTO i420_b VALUES (1,100),(2,200);
INSERT INTO i420_d VALUES (10),(20),(30);
INSERT INTO i420_e VALUES (1,10),(2,20);

-- 1) Non-empty, correct results: scalar subquery must be correlated to the outer row (a.id).
SELECT a.id, a.a_val, b.b_val, d.v
FROM i420_a a
NATURAL JOIN i420_b b
JOIN i420_d d ON d.v = (SELECT e.v FROM i420_e e WHERE e.id = a.id)
ORDER BY a.id;

-- 2) `SELECT *` must not expose internal `__tipg_subquery_*` columns (even with LIMIT 0).
SELECT *
FROM i420_a a
NATURAL JOIN i420_b b
JOIN i420_d d ON d.v = (SELECT e.v FROM i420_e e WHERE e.id = a.id)
LIMIT 0;

DROP TABLE i420_a;
DROP TABLE i420_b;
DROP TABLE i420_d;
DROP TABLE i420_e;

-- Issue #425 regression:
-- Chained USING/NATURAL JOIN predicates after OUTER JOINs must compare against the merged key
-- (COALESCE semantics), not a single prior table alias (can silently drop matches).

DROP TABLE IF EXISTS cu_a;
DROP TABLE IF EXISTS cu_b;
DROP TABLE IF EXISTS cu_c;

CREATE TABLE cu_a(id INT, a_val INT);
CREATE TABLE cu_b(id INT, b_val INT);
CREATE TABLE cu_c(id INT, c_val INT);

INSERT INTO cu_a VALUES (1,10),(2,20);
INSERT INTO cu_b VALUES (1,100);
INSERT INTO cu_c VALUES (2,200);

SELECT id, a_val, b_val, c_val
FROM cu_a
LEFT JOIN cu_b USING(id)
LEFT JOIN cu_c USING(id)
ORDER BY id;

DROP TABLE cu_a;
DROP TABLE cu_b;
DROP TABLE cu_c;

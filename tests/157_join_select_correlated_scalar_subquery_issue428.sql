-- Issue #428 regression:
-- Correlated scalar subquery in SELECT list with JOIN must produce correct results.
-- The subquery correlates to the outer row (a.id) while a separate JOIN is present.

DROP TABLE IF EXISTS issue428_a;
DROP TABLE IF EXISTS issue428_b;

CREATE TABLE issue428_a(id INT);
CREATE TABLE issue428_b(id INT);

INSERT INTO issue428_a VALUES (1), (2);
INSERT INTO issue428_b VALUES (1), (2);

SELECT a.id,
       (SELECT b.id FROM issue428_b b WHERE b.id = a.id) AS v
FROM issue428_a a
JOIN issue428_b bb ON bb.id = a.id
ORDER BY a.id;

DROP TABLE issue428_a;
DROP TABLE issue428_b;

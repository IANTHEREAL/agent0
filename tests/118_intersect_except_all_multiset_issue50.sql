-- INTERSECT ALL / EXCEPT ALL should use multiset semantics (issue #50).

SELECT column1 FROM (VALUES (1),(1),(1),(2),(2)) AS leftv(column1)
INTERSECT ALL
SELECT column1 FROM (VALUES (1),(3),(1)) AS rightv(column1)
ORDER BY column1;

SELECT column1 FROM (VALUES (1),(1),(1),(2),(2)) AS leftv(column1)
EXCEPT ALL
SELECT column1 FROM (VALUES (1),(3),(1)) AS rightv(column1)
ORDER BY column1;

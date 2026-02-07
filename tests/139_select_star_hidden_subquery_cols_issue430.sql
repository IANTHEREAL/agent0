-- Regression test for issue #430:
-- Internal `__tipg_subquery_*` computed columns must not leak into `SELECT *` output,
-- especially when USING/NATURAL join wildcard expansion is used.

DROP TABLE IF EXISTS ha_a;
DROP TABLE IF EXISTS ha_b;
DROP TABLE IF EXISTS ha_d;
DROP TABLE IF EXISTS ha_e;

CREATE TABLE ha_a(id INT, a_val INT);
CREATE TABLE ha_b(id INT, b_val INT);
CREATE TABLE ha_d(v INT);
CREATE TABLE ha_e(id INT, v INT);

INSERT INTO ha_a VALUES (1,10),(2,20);
INSERT INTO ha_b VALUES (1,100),(2,200);
INSERT INTO ha_d VALUES (10),(20);
INSERT INTO ha_e VALUES (1,10),(2,20);

SELECT *
FROM ha_a
JOIN ha_b USING(id)
JOIN ha_d ON ha_d.v = (SELECT v FROM ha_e WHERE ha_e.id = ha_a.id)
LIMIT 0;

DROP TABLE ha_a;
DROP TABLE ha_b;
DROP TABLE ha_d;
DROP TABLE ha_e;

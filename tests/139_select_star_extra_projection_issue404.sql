-- Regression test for issue #404:
-- `SELECT *` plus additional projection items over a USING/NATURAL join must not drop the
-- non-wildcard items (result shape must include them).

DROP TABLE IF EXISTS i404_a;
DROP TABLE IF EXISTS i404_b;

CREATE TABLE i404_a(id INT, a_val INT);
CREATE TABLE i404_b(id INT, b_val INT);

INSERT INTO i404_a VALUES (1,10);
INSERT INTO i404_b VALUES (1,100);

SELECT *, 42 AS x FROM i404_a JOIN i404_b USING(id);

DROP TABLE i404_a;
DROP TABLE i404_b;

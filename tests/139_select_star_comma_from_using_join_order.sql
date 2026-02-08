-- Regression test for issue #404:
-- `SELECT *` over comma FROM + USING/NATURAL join groups must preserve the correct
-- wildcard output order and value mapping.

DROP TABLE IF EXISTS i_misorder_a;
DROP TABLE IF EXISTS i_misorder_b;
DROP TABLE IF EXISTS i_misorder_c;

CREATE TABLE i_misorder_a(id INT, a_val INT);
CREATE TABLE i_misorder_b(id INT, b_val INT);
CREATE TABLE i_misorder_c(id INT, c_val INT);

INSERT INTO i_misorder_a VALUES (1,10);
INSERT INTO i_misorder_b VALUES (1,100);
INSERT INTO i_misorder_c VALUES (1,1000);

SELECT * FROM i_misorder_a, i_misorder_b JOIN i_misorder_c USING(id);

DROP TABLE i_misorder_a;
DROP TABLE i_misorder_b;
DROP TABLE i_misorder_c;

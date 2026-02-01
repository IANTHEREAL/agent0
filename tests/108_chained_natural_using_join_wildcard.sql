-- Regression tests for issue #142: chained NATURAL/USING join wildcard projection

DROP TABLE IF EXISTS a_using;
DROP TABLE IF EXISTS b_using;
DROP TABLE IF EXISTS c_using;

CREATE TABLE a_using (id INT, foo INT, a1 TEXT);
CREATE TABLE b_using (id INT, b1 TEXT);
CREATE TABLE c_using (foo INT, c1 TEXT);

INSERT INTO a_using VALUES (1, 10, 'a1');
INSERT INTO b_using VALUES (1, 'b1');
INSERT INTO c_using VALUES (10, 'c1');

-- Join keys differ across the chain: id is merged by the first join, foo by the second.
SELECT * FROM a_using JOIN b_using USING (id) JOIN c_using USING (foo) ORDER BY foo, id;

DROP TABLE IF EXISTS a_nat;
DROP TABLE IF EXISTS b_nat;
DROP TABLE IF EXISTS c_nat;

CREATE TABLE a_nat (id INT, name TEXT, a1 TEXT);
CREATE TABLE b_nat (id INT, name TEXT, b1 TEXT);
CREATE TABLE c_nat (id INT, c1 TEXT);

INSERT INTO a_nat VALUES (1, 'alice', 'a1');
INSERT INTO b_nat VALUES (1, 'alice', 'b1');
INSERT INTO c_nat VALUES (1, 'c1');

-- The first NATURAL join merges (id, name); the second merges only (id).
SELECT * FROM a_nat NATURAL JOIN b_nat NATURAL JOIN c_nat ORDER BY id;

DROP TABLE a_using;
DROP TABLE b_using;
DROP TABLE c_using;
DROP TABLE a_nat;
DROP TABLE b_nat;
DROP TABLE c_nat;

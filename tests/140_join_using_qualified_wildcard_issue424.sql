-- Regression for #424 (P0): JOIN USING + qualified wildcard (t.*) must not crash pgwire.

DROP TABLE IF EXISTS t140_issue424_a;
DROP TABLE IF EXISTS t140_issue424_b;

CREATE TABLE t140_issue424_a(id INT, a_val INT);
CREATE TABLE t140_issue424_b(id INT, b_val INT);

INSERT INTO t140_issue424_a VALUES (1,10),(2,20);
INSERT INTO t140_issue424_b VALUES (1,100),(2,200);

SELECT t140_issue424_a.* FROM t140_issue424_a JOIN t140_issue424_b USING(id) ORDER BY 1;
SELECT t140_issue424_b.* FROM t140_issue424_a JOIN t140_issue424_b USING(id) ORDER BY 1;

DROP TABLE t140_issue424_a;
DROP TABLE t140_issue424_b;

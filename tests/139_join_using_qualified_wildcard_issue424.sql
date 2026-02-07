-- Regression test for issue #424: JOIN USING + qualified wildcard must not crash pgwire.

DROP TABLE IF EXISTS qa_a2;
DROP TABLE IF EXISTS qa_b2;

CREATE TABLE qa_a2(id INT, a_val INT);
CREATE TABLE qa_b2(id INT, b_val INT);
INSERT INTO qa_a2 VALUES (1,10),(2,20);
INSERT INTO qa_b2 VALUES (1,100),(2,200);

SELECT qa_a2.* FROM qa_a2 JOIN qa_b2 USING(id) ORDER BY 1;

DROP TABLE qa_a2;
DROP TABLE qa_b2;

-- Regression test for issue #427:
-- JOIN regression: NestedJoin (parenthesized join) in FROM should not hard-error.

DROP TABLE IF EXISTS nf_t1;
DROP TABLE IF EXISTS nf_t2;
DROP TABLE IF EXISTS nf_t3;

CREATE TABLE nf_t1(id INT);
CREATE TABLE nf_t2(id INT);
CREATE TABLE nf_t3(id INT);

INSERT INTO nf_t1 VALUES (1);
INSERT INTO nf_t2 VALUES (1);
INSERT INTO nf_t3 VALUES (1);

SELECT * FROM (nf_t1 JOIN nf_t2 ON nf_t1.id = nf_t2.id) j JOIN nf_t3 ON true;

DROP TABLE nf_t1;
DROP TABLE nf_t2;
DROP TABLE nf_t3;

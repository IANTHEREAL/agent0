-- Regression test for issue #13: JOIN condition must be applied for later FROM items.

DROP TABLE IF EXISTS issue13_a CASCADE;
DROP TABLE IF EXISTS issue13_b CASCADE;
DROP TABLE IF EXISTS issue13_c CASCADE;

CREATE TABLE issue13_a(id INT);
CREATE TABLE issue13_b(id INT);
CREATE TABLE issue13_c(id INT);

INSERT INTO issue13_a VALUES (1), (2);
INSERT INTO issue13_b VALUES (1), (2);
INSERT INTO issue13_c VALUES (1);

SELECT count(*) AS issue13_cnt
FROM issue13_a, issue13_b JOIN issue13_c ON issue13_b.id = issue13_c.id;

DROP TABLE issue13_a;
DROP TABLE issue13_b;
DROP TABLE issue13_c;

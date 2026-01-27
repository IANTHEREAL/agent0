-- Issue #13 regression: JOIN constraints must be applied for non-first FROM items.

DROP TABLE IF EXISTS issue13_a;
DROP TABLE IF EXISTS issue13_b;
DROP TABLE IF EXISTS issue13_c;

CREATE TABLE issue13_a(id INT);
CREATE TABLE issue13_b(id INT);
CREATE TABLE issue13_c(id INT);

INSERT INTO issue13_a VALUES (1), (2);
INSERT INTO issue13_b VALUES (1), (2);
INSERT INTO issue13_c VALUES (1);

SELECT COUNT(*)
FROM issue13_a, issue13_b JOIN issue13_c ON issue13_b.id = issue13_c.id;

SELECT issue13_a.id AS a_id, issue13_b.id AS b_id, issue13_c.id AS c_id
FROM issue13_a, issue13_b JOIN issue13_c ON issue13_b.id = issue13_c.id
ORDER BY a_id, b_id, c_id;

DROP TABLE issue13_a;
DROP TABLE issue13_b;
DROP TABLE issue13_c;

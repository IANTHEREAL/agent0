-- Issue #87 regression:
-- JOIN ... ON subquery references the right-side join alias.
-- Ensure correlation detection includes the right alias and fails closed with a stable error.

DROP TABLE IF EXISTS issue87_l;
DROP TABLE IF EXISTS issue87_r;
DROP TABLE IF EXISTS issue87_s;

CREATE TABLE issue87_l(id INT);
CREATE TABLE issue87_r(id INT);
CREATE TABLE issue87_s(id INT);

INSERT INTO issue87_l VALUES (1), (2);
INSERT INTO issue87_r VALUES (1), (2);
INSERT INTO issue87_s VALUES (1), (2);

SELECT l.id
FROM issue87_l l
JOIN issue87_r r
  ON l.id = (SELECT max(s.id) FROM issue87_s s WHERE s.id = r.id)
ORDER BY l.id;

DROP TABLE issue87_l;
DROP TABLE issue87_r;
DROP TABLE issue87_s;

-- Issue #446 regression:
-- Correlated scalar subquery outer references must be detected/substituted in JOIN ... ON.

DROP TABLE IF EXISTS issue446_outer;
DROP TABLE IF EXISTS issue446_inner;
DROP TABLE IF EXISTS issue446_keys;

CREATE TABLE issue446_outer (id INT, vv INT);
CREATE TABLE issue446_inner (id INT, vv INT);
CREATE TABLE issue446_keys (id INT);

INSERT INTO issue446_outer VALUES (1, 10), (2, 20);
INSERT INTO issue446_inner VALUES (1, 10), (2, 20);
INSERT INTO issue446_keys VALUES (1), (2);

-- Outer reference appears only in the subquery JOIN condition.
SELECT (
  SELECT i.vv
  FROM issue446_inner i
  JOIN issue446_keys k ON k.id = o.id AND k.id = i.id
) AS vv
FROM issue446_outer o
ORDER BY vv;

DROP TABLE issue446_outer;
DROP TABLE issue446_inner;
DROP TABLE issue446_keys;


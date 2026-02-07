-- Regression: qualified GROUP BY column should not break projection resolution
-- Repro shape: SELECT age, COUNT(*) FROM t GROUP BY t.age

DROP TABLE IF EXISTS d2_group_by;
CREATE TABLE d2_group_by (age INT);
INSERT INTO d2_group_by(age) VALUES (10), (10), (20);

SELECT age, COUNT(*) AS cnt
FROM d2_group_by AS t
GROUP BY t.age
ORDER BY age;

SELECT "age", COUNT(*) AS cnt
FROM d2_group_by AS "t"
GROUP BY "t"."age"
ORDER BY "age";

DROP TABLE d2_group_by;


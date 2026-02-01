-- Issue #86 regression (follow-up to #223):
-- RIGHT/FULL OUTER JOIN with USING must treat the merged join key
-- like COALESCE(left.a, right.a) for unqualified references (SELECT/ORDER BY/GROUP BY).

DROP TABLE IF EXISTS join_using_left;
DROP TABLE IF EXISTS join_using_right;

CREATE TABLE join_using_left (l INT, a INT);
CREATE TABLE join_using_right (a INT, r INT);

INSERT INTO join_using_left VALUES (10, 1), (30, 3);
INSERT INTO join_using_right VALUES (1, 100), (2, 200);

-- Unmatched right rows should output `a` from the right table, not NULL.
SELECT a
FROM join_using_left RIGHT JOIN join_using_right USING (a)
ORDER BY a;

SELECT join_using_left.a AS l_a, join_using_right.a AS r_a
FROM join_using_left RIGHT JOIN join_using_right USING (a)
ORDER BY a;

-- FULL OUTER JOIN should output COALESCE(left.a, right.a) for the merged `a`.
SELECT a
FROM join_using_left FULL JOIN join_using_right USING (a)
ORDER BY a;

SELECT join_using_left.a AS l_a, join_using_right.a AS r_a
FROM join_using_left FULL JOIN join_using_right USING (a)
ORDER BY a;

SELECT a
FROM join_using_left FULL JOIN join_using_right USING (a)
GROUP BY a
ORDER BY a;

-- Window ORDER BY inside OVER(...) should also use the merged key.
SELECT a, row_number() OVER (ORDER BY a NULLS FIRST) AS rn
FROM join_using_left FULL JOIN join_using_right USING (a)
ORDER BY a;

-- DISTINCT ON should not treat right-only merged keys as NULL.
INSERT INTO join_using_left VALUES (40, NULL);
SELECT DISTINCT ON (a) a, l, r
FROM join_using_left FULL JOIN join_using_right USING (a)
ORDER BY a NULLS FIRST;

DROP TABLE join_using_left;
DROP TABLE join_using_right;


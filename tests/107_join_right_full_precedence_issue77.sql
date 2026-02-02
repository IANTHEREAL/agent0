-- Issues #77/#83 regression: explicit RIGHT/FULL JOIN must bind tighter than comma join.
-- `FROM a, b RIGHT JOIN c ON ...` must behave like `a CROSS JOIN (b RIGHT JOIN c ...)`.

DROP TABLE IF EXISTS issue77_a;
DROP TABLE IF EXISTS issue77_b;
DROP TABLE IF EXISTS issue77_c;

CREATE TABLE issue77_a(a_id INT);
CREATE TABLE issue77_b(b_id INT);
CREATE TABLE issue77_c(c_id INT);

INSERT INTO issue77_a VALUES (1), (2);
INSERT INTO issue77_b VALUES (1);
INSERT INTO issue77_c VALUES (2); -- no match with b

-- Unmatched RIGHT rows must not NULL-extend preceding comma-FROM tables and must be replicated.
SELECT issue77_a.a_id AS a_id, issue77_b.b_id AS b_id, issue77_c.c_id AS c_id
FROM issue77_a, issue77_b RIGHT JOIN issue77_c ON issue77_b.b_id = issue77_c.c_id
ORDER BY issue77_a.a_id, COALESCE(issue77_b.b_id, 2147483647), COALESCE(issue77_c.c_id, 2147483647);

-- If the preceding comma-FROM table is empty, the overall result must be empty (CROSS JOIN semantics).
TRUNCATE TABLE issue77_a;

SELECT issue77_a.a_id AS a_id, issue77_b.b_id AS b_id, issue77_c.c_id AS c_id
FROM issue77_a, issue77_b RIGHT JOIN issue77_c ON issue77_b.b_id = issue77_c.c_id
ORDER BY issue77_a.a_id, COALESCE(issue77_b.b_id, 2147483647), COALESCE(issue77_c.c_id, 2147483647);

-- FULL OUTER JOIN: unmatched rows on either side must not NULL-extend `issue77_a` and must be replicated.
INSERT INTO issue77_a VALUES (1), (2);

SELECT issue77_a.a_id AS a_id, issue77_b.b_id AS b_id, issue77_c.c_id AS c_id
FROM issue77_a, issue77_b FULL OUTER JOIN issue77_c ON issue77_b.b_id = issue77_c.c_id
ORDER BY issue77_a.a_id, COALESCE(issue77_b.b_id, 2147483647), COALESCE(issue77_c.c_id, 2147483647);

DROP TABLE issue77_a;
DROP TABLE issue77_b;
DROP TABLE issue77_c;


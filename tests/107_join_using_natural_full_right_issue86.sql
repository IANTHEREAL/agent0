-- Issue #86 regression: FULL/RIGHT JOIN USING/NATURAL merged join key should behave like
-- COALESCE(left.id, right.id) and must not pull values from unrelated FROM items.

DROP TABLE IF EXISTS issue86_b;
DROP TABLE IF EXISTS issue86_c;
DROP TABLE IF EXISTS issue86_extra;

CREATE TABLE issue86_b(id INT);
CREATE TABLE issue86_c(id INT);
CREATE TABLE issue86_extra(id INT);

INSERT INTO issue86_b VALUES (1), (NULL);
INSERT INTO issue86_c VALUES (1), (2), (NULL);
INSERT INTO issue86_extra VALUES (999);

-- USING: merged `id` should behave like COALESCE(issue86_b.id, issue86_c.id) for outer rows.
SELECT id, issue86_b.id AS b_id, issue86_c.id AS c_id
FROM issue86_b FULL JOIN issue86_c USING (id)
ORDER BY c_id;

SELECT id, issue86_b.id AS b_id, issue86_c.id AS c_id
FROM issue86_b RIGHT JOIN issue86_c USING (id)
ORDER BY c_id;

-- NATURAL: same merged key behavior as USING for common column `id`.
SELECT id, issue86_b.id AS b_id, issue86_c.id AS c_id
FROM issue86_b NATURAL FULL JOIN issue86_c
ORDER BY c_id;

SELECT id, issue86_b.id AS b_id, issue86_c.id AS c_id
FROM issue86_b NATURAL RIGHT JOIN issue86_c
ORDER BY c_id;

-- Extra FROM table with the same column name: merged `id` must not pull from issue86_extra.id.
SELECT id, issue86_b.id AS b_id, issue86_c.id AS c_id, issue86_extra.id AS extra_id
FROM issue86_b FULL JOIN issue86_c USING (id), issue86_extra
WHERE issue86_b.id IS NULL AND issue86_c.id IS NULL
ORDER BY extra_id;

-- ORDER BY: ambiguous unqualified join key must error (not resolve via merged-key COALESCE).
SELECT 1 FROM issue86_b FULL JOIN issue86_c USING (id), issue86_extra ORDER BY id;

DROP TABLE issue86_b;
DROP TABLE issue86_c;
DROP TABLE issue86_extra;

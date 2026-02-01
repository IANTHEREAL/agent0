-- Issue #55 regression: SELECT * with USING/NATURAL + RIGHT/FULL must keep column/value alignment
-- and merged join key should behave like COALESCE(left.id, right.id) for outer rows.

DROP TABLE IF EXISTS issue55_left;
DROP TABLE IF EXISTS issue55_right;
DROP TABLE IF EXISTS issue55_extra;

-- Left table puts the join key (id) AFTER a non-key column to exercise misalignment bugs.
CREATE TABLE issue55_left (x TEXT, id INT);
CREATE TABLE issue55_right (id INT, y TEXT);
CREATE TABLE issue55_extra (id INT);

INSERT INTO issue55_left VALUES ('lx1', 1), ('lx_null', NULL);
INSERT INTO issue55_right VALUES (1, 'ry1'), (2, 'ry2'), (NULL, 'ry_null');
INSERT INTO issue55_extra VALUES (999);

-- USING: merged `id` should behave like COALESCE(issue55_left.id, issue55_right.id) for outer rows.
SELECT *
FROM issue55_left FULL JOIN issue55_right USING (id)
ORDER BY id NULLS FIRST, x, y;

SELECT *
FROM issue55_left RIGHT JOIN issue55_right USING (id)
ORDER BY id NULLS FIRST, x, y;

-- NATURAL: same merged key behavior as USING for common column `id`.
SELECT *
FROM issue55_left NATURAL FULL JOIN issue55_right
ORDER BY id NULLS FIRST, x, y;

SELECT *
FROM issue55_left NATURAL RIGHT JOIN issue55_right
ORDER BY id NULLS FIRST, x, y;

-- Extra FROM table with the same column name: the JOIN-merged `id` must not pull from issue55_extra.id.
SELECT *
FROM issue55_left FULL JOIN issue55_right USING (id), issue55_extra
WHERE issue55_left.id IS NULL AND issue55_right.id IS NULL
ORDER BY issue55_extra.id, x, y;

DROP TABLE issue55_left;
DROP TABLE issue55_right;
DROP TABLE issue55_extra;

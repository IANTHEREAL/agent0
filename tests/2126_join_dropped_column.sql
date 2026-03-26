-- Issue #2126: JOIN on a table with a dropped column caused
-- "column index N out of bounds" because the analyzer added a
-- phantom ctid system column to the scope in SELECT context.

DROP TABLE IF EXISTS t2126_a;
DROP TABLE IF EXISTS t2126_b;

CREATE TABLE t2126_a (id INT PRIMARY KEY, x TEXT, y TEXT);
CREATE TABLE t2126_b (id INT PRIMARY KEY, val TEXT);

INSERT INTO t2126_a VALUES (1, 'hello', 'world'), (2, 'foo', 'bar');
INSERT INTO t2126_b VALUES (1, 'alpha'), (2, 'beta');

-- Drop a column to trigger the dropped-column code path.
ALTER TABLE t2126_a DROP COLUMN y;

-- 1) SELECT with explicit column JOIN.
SELECT a.id, a.x, b.val
FROM t2126_a a
JOIN t2126_b b ON a.id = b.id
ORDER BY a.id;

-- 2) SELECT * through a JOIN.
SELECT *
FROM t2126_a a
JOIN t2126_b b ON a.id = b.id
ORDER BY a.id;

-- 3) LEFT JOIN LATERAL.
SELECT a.id, a.x, sub.cnt
FROM t2126_a a
LEFT JOIN LATERAL (
    SELECT COUNT(*) AS cnt FROM t2126_b b WHERE b.id = a.id
) sub ON true
ORDER BY a.id;

-- 4) UPDATE ... FROM with dropped-column table in JOIN.
--    The DML path needs ctid; verify scope is correct.
UPDATE t2126_b
SET val = 'updated'
FROM t2126_a a
WHERE t2126_b.id = a.id AND a.x = 'hello';

SELECT id, val FROM t2126_b ORDER BY id;

-- 5) DELETE ... USING with dropped-column table in JOIN.
DELETE FROM t2126_b
USING t2126_a a
WHERE t2126_b.id = a.id AND a.x = 'foo';

SELECT id, val FROM t2126_b ORDER BY id;

-- 6) Self-join on table with dropped columns.
SELECT a.id AS a_id, b.id AS b_id, a.x, b.x AS bx
FROM t2126_a a
JOIN t2126_a b ON a.id = b.id
ORDER BY a.id;

DROP TABLE t2126_b;
DROP TABLE t2126_a;

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

-- This JOIN must work even though t2126_a has a dropped column.
SELECT a.id, a.x, b.val
FROM t2126_a a
JOIN t2126_b b ON a.id = b.id
ORDER BY a.id;

-- SELECT * through a JOIN must also work.
SELECT *
FROM t2126_a a
JOIN t2126_b b ON a.id = b.id
ORDER BY a.id;

-- LEFT JOIN LATERAL must also work.
SELECT a.id, a.x, sub.cnt
FROM t2126_a a
LEFT JOIN LATERAL (
    SELECT COUNT(*) AS cnt FROM t2126_b b WHERE b.id = a.id
) sub ON true
ORDER BY a.id;

DROP TABLE t2126_b;
DROP TABLE t2126_a;

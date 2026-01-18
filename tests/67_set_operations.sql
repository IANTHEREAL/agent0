-- Set Operations Test
-- Tests UNION, INTERSECT, EXCEPT operations

-- Setup test tables
DROP TABLE IF EXISTS set_a CASCADE;
DROP TABLE IF EXISTS set_b CASCADE;

CREATE TABLE set_a (id INT PRIMARY KEY, val TEXT);
CREATE TABLE set_b (id INT PRIMARY KEY, val TEXT);

INSERT INTO set_a VALUES (1, 'apple'), (2, 'banana'), (3, 'cherry');
INSERT INTO set_b VALUES (2, 'banana'), (3, 'cherry'), (4, 'date');

-- UNION (removes duplicates)
SELECT id, val FROM set_a
UNION
SELECT id, val FROM set_b
ORDER BY id;

-- UNION ALL (keeps duplicates)
SELECT id, val FROM set_a
UNION ALL
SELECT id, val FROM set_b
ORDER BY id;

-- INTERSECT (common elements)
SELECT id, val FROM set_a
INTERSECT
SELECT id, val FROM set_b
ORDER BY id;

-- EXCEPT (elements in first but not in second)
SELECT id, val FROM set_a
EXCEPT
SELECT id, val FROM set_b
ORDER BY id;

-- Multiple UNIONs
SELECT id, val FROM set_a WHERE id = 1
UNION
SELECT id, val FROM set_a WHERE id = 2
UNION
SELECT id, val FROM set_b WHERE id = 4
ORDER BY id;

-- UNION with different column names (uses first query's names)
SELECT id AS item_id, val AS item_val FROM set_a WHERE id <= 2
UNION
SELECT id, val FROM set_b WHERE id >= 3
ORDER BY item_id;

-- UNION with expressions
SELECT id, UPPER(val) AS val FROM set_a
UNION
SELECT id, LOWER(val) AS val FROM set_b
ORDER BY id;

-- Complex: UNION inside subquery
SELECT * FROM (
    SELECT id, val FROM set_a
    UNION
    SELECT id, val FROM set_b
) AS combined
WHERE id > 1
ORDER BY id;

-- Cleanup
DROP TABLE set_a;
DROP TABLE set_b;

SELECT 'Set operations tests completed' AS result;

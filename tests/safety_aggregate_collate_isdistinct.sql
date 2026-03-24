-- Regression guard for PR #1967: TypedExprKind wildcard elimination.
-- Validates that Collate-wrapped and IsDistinctFrom-wrapped aggregates
-- are correctly detected and rewritten.

-- ================================================================
-- Setup
-- ================================================================
DROP TABLE IF EXISTS agg_collate_test;
CREATE TABLE agg_collate_test (
    id INT PRIMARY KEY,
    dept TEXT NOT NULL,
    name TEXT NOT NULL,
    val INT
);
INSERT INTO agg_collate_test VALUES
    (1, 'eng', 'Charlie', 10),
    (2, 'eng', 'Alice', 20),
    (3, 'eng', 'Bob', 30),
    (4, 'sales', 'Zara', 40),
    (5, 'sales', 'Amy', 50);

-- ================================================================
-- Test 1: string_agg() COLLATE "C" in SELECT (the exact bug case)
-- ================================================================
SELECT dept, string_agg(name, ',' ORDER BY name) COLLATE "C" AS names
FROM agg_collate_test
GROUP BY dept
ORDER BY dept;

-- ================================================================
-- Test 2: Aggregate in HAVING with COLLATE
-- ================================================================
SELECT dept, string_agg(name, ',' ORDER BY name) COLLATE "C" AS names
FROM agg_collate_test
GROUP BY dept
HAVING string_agg(name, ',' ORDER BY name) COLLATE "C" LIKE 'A%'
ORDER BY dept;

-- ================================================================
-- Test 3: IS DISTINCT FROM in SELECT
-- ================================================================
SELECT 1 IS DISTINCT FROM NULL AS t1;
SELECT NULL IS DISTINCT FROM NULL AS t2;
SELECT 1 IS DISTINCT FROM 1 AS t3;
SELECT NULL IS NOT DISTINCT FROM NULL AS t4;
SELECT 1 IS NOT DISTINCT FROM NULL AS t5;

-- ================================================================
-- Test 4: IS DISTINCT FROM in WHERE clause
-- ================================================================
SELECT id, val FROM agg_collate_test
WHERE val IS DISTINCT FROM 20
ORDER BY id;

-- ================================================================
-- Test 5: IS DISTINCT FROM in GROUP BY / aggregate context
-- ================================================================
SELECT
    (val IS DISTINCT FROM NULL) AS has_val,
    count(*) AS cnt
FROM agg_collate_test
GROUP BY (val IS DISTINCT FROM NULL)
ORDER BY has_val;

-- ================================================================
-- Test 6: count(*) + IS DISTINCT FROM in HAVING
-- Note: count(*) returns Int64, literal 0 is Int32.
-- IS DISTINCT FROM lacks coercion (#2065), so cast explicitly.
-- ================================================================
SELECT dept, count(*) AS cnt
FROM agg_collate_test
GROUP BY dept
HAVING count(*) IS DISTINCT FROM 0::bigint
ORDER BY dept;

-- ================================================================
-- Cleanup
-- ================================================================
DROP TABLE agg_collate_test;

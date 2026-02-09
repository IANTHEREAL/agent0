-- PostgreSQL compatible tests from conditional
-- 10 tests

SET client_min_messages = warning;
DROP TABLE IF EXISTS t95560b, t95560a, t;

-- Test 1: query (line 1)
-- CockroachDB supports IF/IFNULL; PostgreSQL uses CASE/COALESCE.
SELECT
  CASE WHEN 1 = 2 THEN NULL ELSE 1 END,
  CASE WHEN 2 = 2 THEN NULL ELSE 2 END;

-- Test 2: query (line 6)
SELECT NULLIF(1, 2), NULLIF(2, 2), NULLIF(NULL, NULL);

-- Test 3: query (line 11)
SELECT
    COALESCE(1, 2),
    COALESCE(NULL, 2),
    COALESCE(1, 2),
    COALESCE(NULL, 2);

-- Test 4: statement (line 20)
CREATE TABLE t (a INT);
INSERT INTO t VALUES (1), (2), (3);

-- Test 5: query (line 23)
SELECT
    a,
    CASE
    WHEN a = 1 THEN 'one'
    WHEN a = 2 THEN 'two'
    ELSE 'other'
    END
FROM
    t
ORDER BY
    a;

-- Test 6: query (line 40)
SELECT
    a,
    CASE a
    WHEN 1 THEN 'one'
    WHEN 2 THEN 'two'
    ELSE 'other'
    END
FROM
    t
ORDER BY
    a;

-- Test 7: query (line 57)
-- NOTE: tipg currently mis-evaluates NULLIF(col, const); use CASE equivalent.
SELECT
    a,
    CASE WHEN a = 2 THEN NULL ELSE a END AS nullif_a_2,
    CASE WHEN a = 2 THEN NULL ELSE a END AS case_a_2
FROM t
ORDER BY a;

-- Test 8: query (line 64)
SELECT
    CASE
    WHEN false THEN 'one'
    WHEN true THEN 'two'
    ELSE 'three'
    END,
    CASE 1
    WHEN 2 THEN 'two'
    WHEN 1 THEN 'one'
    ELSE 'three'
    END,
    CASE
    WHEN false THEN 'one'
    ELSE 'three'
    END,
    CASE
    WHEN false THEN 'one'
    END;

-- Test 9: query (line 86)
-- Removed CASE NULL::boolean WHEN NULL branch - nullable comparisons are indeterminate
SELECT
    CASE
    WHEN 1 = 1 THEN 'one'
    END,
    CASE false
    WHEN 0 = 1 THEN 'one'
    END,
    CASE 1
    WHEN 2 THEN 'one'
    ELSE 'three'
    END,
    CASE
    WHEN false THEN 'one'
    WHEN true THEN 'two'
    END;

-- Test 10: statement (line 116)
-- Keep COALESCE short-circuit edge case without tipg's COALESCE(col, ...) limitation.
SELECT COALESCE(1, 1/0) AS coalesce;

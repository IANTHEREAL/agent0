-- Issue #2136: Systemic comparison coercion via ensure_comparison_compatible.
-- Validates that ALL(), NULLIF(), and IN (SELECT ...) apply implicit casts
-- between compatible types, matching PostgreSQL behaviour.

DROP TABLE IF EXISTS t_2136 CASCADE;
CREATE TABLE t_2136 (id INT, val BIGINT, name TEXT);
INSERT INTO t_2136 VALUES (1, 100, 'alice'), (2, 200, 'bob'), (3, 300, 'carol');

-- ============================================================
-- 1. ALL() with mixed-type ARRAY elements (int32 col vs int64 array)
-- ============================================================

-- int32 <> ALL(int64 array): coercion should promote int32 -> int64
SELECT id FROM t_2136 WHERE id <> ALL(ARRAY[2::bigint, 3::bigint]) ORDER BY id;

-- int32 > ALL(int64 array): comparison operators also need coercion
SELECT id FROM t_2136 WHERE id > ALL(ARRAY[1::bigint, 2::bigint]) ORDER BY id;

-- bigint column <> ALL(int32 array): reverse direction
SELECT val FROM t_2136 WHERE val <> ALL(ARRAY[100::int, 200::int]) ORDER BY val;

-- ============================================================
-- 2. NULLIF with cross-type arguments
-- ============================================================

-- NULLIF(int, bigint): should coerce int->bigint for comparison; values equal -> NULL
SELECT NULLIF(1, 1::bigint);

-- NULLIF(bigint, int): reverse; values equal -> NULL
SELECT NULLIF(100::bigint, 100);

-- NULLIF(int, bigint) where values differ: returns first arg
SELECT NULLIF(1, 2::bigint);

-- NULLIF(float8, int): numeric promotion; values differ -> returns first arg
SELECT NULLIF(1.5, 1);

-- NULLIF(int, float8): numeric promotion; values differ -> returns first arg
SELECT NULLIF(1, 1.5);

-- NULLIF with NULL: should work without coercion
SELECT NULLIF(1, NULL);
SELECT NULLIF(NULL::int, 1);

-- ============================================================
-- 3. IN (SELECT ...) with cross-type comparison
-- ============================================================

-- int32 IN (SELECT bigint): LHS should be coerced to bigint
SELECT name FROM t_2136
WHERE id IN (SELECT 1::bigint UNION ALL SELECT 2::bigint)
ORDER BY name;

DROP TABLE t_2136;

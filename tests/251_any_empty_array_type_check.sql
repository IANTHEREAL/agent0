-- Issue #1059/#1125: empty array in ANY/= ANY must still type-check LHS
-- against the array element type, and must still evaluate LHS (runtime errors).

-- Non-castable text vs int[]: runtime cast error (PG: invalid input syntax)
SELECT 'hello' <> ANY(ARRAY[]::int[]) AS res;

-- Erroring LHS with empty array: 1/0 must still be evaluated (runtime error)
SELECT 1/0 <> ANY(ARRAY[]::int[]) AS res;

-- = ANY: non-castable text vs int[] gives runtime cast error
SELECT 'hello' = ANY(ARRAY[]::int[]) AS res;

-- = ANY: erroring LHS with empty array
SELECT 1/0 = ANY(ARRAY[]::int[]) AS res;

-- #1125 P2: NULL LHS + empty array is vacuously false (PG parity)
SELECT NULL::int = ANY(ARRAY[]::int[]) AS res;

SELECT NULL::int <> ANY(ARRAY[]::int[]) AS res;

-- #1125 P1: Castable literal succeeds and returns false
SELECT '1' = ANY(ARRAY[]::int[]) AS res;

SELECT '1' <> ANY(ARRAY[]::int[]) AS res;

-- #1125 P1 regression guard: explicit text/name must not be coerced
SELECT '1'::text = ANY(ARRAY[]::int[]) AS res;

SELECT '1'::name = ANY(ARRAY[]::int[]) AS res;

SELECT '1'::text <> ANY(ARRAY[]::int[]) AS res;

SELECT '1'::name <> ANY(ARRAY[]::int[]) AS res;

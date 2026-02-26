-- Issue #1059: empty array in ANY/= ANY must still type-check LHS against
-- the array element type, and must still evaluate LHS (runtime errors).

-- Type mismatch: text vs int[] must error (analyzer rejects)
SELECT 'hello' <> ANY(ARRAY[]::int[]) AS res;

-- Erroring LHS with empty array: 1/0 must still be evaluated (runtime error)
SELECT 1/0 <> ANY(ARRAY[]::int[]) AS res;

-- = ANY: same type mismatch must error
SELECT 'hello' = ANY(ARRAY[]::int[]) AS res;

-- = ANY: erroring LHS with empty array
SELECT 1/0 = ANY(ARRAY[]::int[]) AS res;

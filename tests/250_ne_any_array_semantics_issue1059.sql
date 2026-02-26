-- Issue #1059: `x <> ANY(ARRAY[...])` must use OR semantics, not NOT IN (AND).
-- `x <> ANY(ARRAY[a,b,c])` = (x<>a) OR (x<>b) OR (x<>c)

-- TRUE: 1<>2 is true, so the OR chain is true
SELECT 1 <> ANY(ARRAY[1,2,3]) AS res;

-- FALSE: 1<>1 is false for all elements
SELECT 1 <> ANY(ARRAY[1,1,1]) AS res;

-- NULL: NULL<>1 is NULL, NULL<>2 is NULL => NULL OR NULL = NULL
SELECT NULL::int <> ANY(ARRAY[1,2]) AS res;

-- Empty array: ANY of empty set is FALSE (LHS is still evaluated)
SELECT 1 <> ANY(ARRAY[]::int[]) AS res;

-- Cross-type: integer <> numeric coercion (1<>1.0 false, 1<>2.5 true => TRUE)
SELECT 1 <> ANY(ARRAY[1.0, 2.5]) AS res;

-- Cross-type all equal after coercion (1<>1.0 false for all => FALSE)
SELECT 1 <> ANY(ARRAY[1.0, 1.0, 1.0]) AS res;

-- NULL in array: 1<>2 = TRUE (short-circuit, ignores NULL element)
SELECT 1 <> ANY(ARRAY[2, NULL]) AS res;

-- NULL in array, no TRUE: 1<>1 = FALSE, 1<>NULL = NULL => FALSE OR NULL = NULL
SELECT 1 <> ANY(ARRAY[1, NULL]) AS res;

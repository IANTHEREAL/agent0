-- Issue #43 regression: ANY/ALL must follow SQL three-valued logic (3VL) with NULL.

-- ANY: TRUE dominates; otherwise NULL if any NULL; otherwise FALSE.
SELECT 1 = ANY(ARRAY[NULL]::int[]) AS res;
SELECT 1 = ANY(ARRAY[2, NULL]::int[]) AS res;
SELECT 1 = ANY(ARRAY[1, NULL]::int[]) AS res;
SELECT NULL::int = ANY(ARRAY[1]::int[]) AS res;

-- ALL: FALSE dominates; otherwise NULL if any NULL; otherwise TRUE.
SELECT 1 = ALL(ARRAY[NULL]::int[]) AS res;
SELECT 1 = ALL(ARRAY[1, NULL]::int[]) AS res;
SELECT 1 = ALL(ARRAY[1, 2, NULL]::int[]) AS res;
SELECT 1 = ALL(ARRAY[1, 1]::int[]) AS res;
SELECT NULL::int = ALL(ARRAY[1]::int[]) AS res;

-- Empty array edge cases: ANY -> FALSE, ALL -> TRUE.
SELECT 1 = ANY(ARRAY[]::int[]) AS res;
SELECT 1 = ALL(ARRAY[]::int[]) AS res;


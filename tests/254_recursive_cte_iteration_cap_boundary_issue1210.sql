-- Regression for #1210: recursive CTE recursion cap must not reject
-- naturally terminating queries at the boundary.

WITH RECURSIVE t(n) AS (
    SELECT 1
    UNION ALL
    SELECT n + 1 FROM t WHERE n < 1000
)
SELECT max(n) AS max_n FROM t;

-- Boundary case that hit the old off-by-one check:
-- the step at iteration 1000 can still yield rows, and the next step terminates.
WITH RECURSIVE t(n) AS (
    SELECT 1
    UNION ALL
    SELECT n + 1 FROM t WHERE n <= 1000
)
SELECT max(n) AS max_n FROM t;

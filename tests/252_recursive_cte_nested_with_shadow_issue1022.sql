-- Regression test for #1022: recursive CTE self-reference detection
-- with nested WITH shadowing.
--
-- cte_body_references_name() builds a synthetic query with `with: None`.
-- This verifies that:
--   (1) A genuinely recursive CTE is still correctly classified as recursive
--       even when the recursive arm contains a nested WITH clause that shadows
--       the outer CTE name at an inner scope.
--   (2) A CTE in WITH RECURSIVE whose body only references the CTE name
--       inside a nested WITH shadow is correctly classified as non-recursive
--       and executes with correct results.

-- ============================================================
-- Case 1: Recursive CTE with inner WITH shadowing the same name
--
-- The recursive arm references FROM counter (outer working table) AND
-- a joined derived subquery whose own CTE also names 'counter', shadowing
-- the outer name inside that subquery. Binder must detect the top-level
-- self-reference and classify this CTE as recursive.
-- ============================================================
WITH RECURSIVE counter AS (
    SELECT 1 AS n
    UNION ALL
    SELECT counter.n + 1
    FROM counter
    JOIN (
        WITH counter AS (SELECT 100 AS shadow_n)
        SELECT shadow_n FROM counter
    ) shadow_sub ON shadow_sub.shadow_n > 0
    WHERE counter.n < 5
)
SELECT n FROM counter ORDER BY n;

-- ============================================================
-- Case 2: Non-recursive CTE in WITH RECURSIVE block
--
-- The body only references the CTE name 'non_recursive_shadow' inside a
-- nested WITH that shadows it. cte_body_references_name must return false
-- (no top-level self-reference), classifying this as non-recursive.
-- Execution: base {42} UNION ALL arm_once (arm produces empty set because
-- WHERE inner_q.val > 100 is false for val=0) => result is just {42}.
-- ============================================================
WITH RECURSIVE non_recursive_shadow AS (
    SELECT 42 AS val
    UNION ALL
    SELECT inner_q.val + 1
    FROM (
        WITH non_recursive_shadow AS (SELECT 0 AS val)
        SELECT val FROM non_recursive_shadow
    ) inner_q
    WHERE inner_q.val > 100
)
SELECT val FROM non_recursive_shadow ORDER BY val;

-- ============================================================
-- Case 2b: Non-recursive shadow CTE — discriminating variant
--
-- Differs from Case 2 in that the recursive arm always produces a
-- non-empty result {1} regardless of the working table, because the
-- inner shadow always evaluates to SELECT 1 AS val.
--
-- Correct (non-recursive classification):
--   Body executes ONCE: {0} UNION ALL {1} = exactly two rows: {0, 1}.
--
-- Wrong (recursive misclassification):
--   Executor iterates: inner shadow always returns {1}, working table
--   never empties, loop runs until the iteration cap. With LIMIT 5 the
--   outer SELECT would return {0, 1, 1, 1, 1} — five rows instead of two.
--
-- LIMIT 5 prevents infinite output if misclassified.
-- Expected: exactly two rows: 0, 1.
-- ============================================================
WITH RECURSIVE shadow_disc AS (
    SELECT 0 AS val
    UNION ALL
    SELECT inner_q.val
    FROM (
        WITH shadow_disc AS (SELECT 1 AS val)
        SELECT val FROM shadow_disc
    ) inner_q
)
SELECT val FROM shadow_disc ORDER BY val
LIMIT 5;

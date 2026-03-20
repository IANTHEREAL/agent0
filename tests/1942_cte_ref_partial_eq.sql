-- Regression test for #1942: CTE references must not collapse during aggregate dedup.
-- Before the fix, FROM c1 and FROM c2 compared equal (both table_id=0, same schema),
-- causing SUM((SELECT x FROM c1)) and SUM((SELECT x FROM c2)) to share one slot.

-- Test: Two different CTEs referenced in separate aggregates must produce distinct values.
WITH c1 AS (SELECT 1 AS x),
     c2 AS (SELECT 2 AS x)
SELECT SUM((SELECT x FROM c1)), SUM((SELECT x FROM c2));

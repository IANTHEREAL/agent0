-- Regression test for #1572 and #1674: subquery-bearing aggregate expressions
-- Verifies that TypedExprKind derives PartialEq correctly for AnalyzedQuery.
-- Before the fix, AnalyzedQuery::eq always returned false, breaking aggregate
-- slot lookup for expressions containing subqueries.

-- Test: Same subquery aggregate referenced twice should work (dedup to 1 slot)
SELECT SUM((SELECT 1)) + SUM((SELECT 1));

-- Test: Subquery in GROUP BY expression
DROP TABLE IF EXISTS t_test;
CREATE TABLE t_test (id INT, v TEXT);
INSERT INTO t_test VALUES (1, 'a'), (2, 'b');
SELECT (SELECT 1), count(*) FROM t_test GROUP BY (SELECT 1);
DROP TABLE t_test;

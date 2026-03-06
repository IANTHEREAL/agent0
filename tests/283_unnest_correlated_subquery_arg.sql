-- Regression: correlated subquery inside table-function arg must force NLJ.
SELECT t.id, u.val
FROM (VALUES (1),(2)) AS t(id)
JOIN unnest((SELECT ARRAY[t.id])) AS u(val) ON t.id = u.val
ORDER BY t.id;

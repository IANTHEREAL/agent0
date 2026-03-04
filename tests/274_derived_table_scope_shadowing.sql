-- Regression: derived table / CTE aliases must shadow outer relation bindings.
-- Inner alias "n" should block xmin from resolving to outer pg_namespace.

-- Derived table shadowing: n.xmin must NOT pierce through to outer pg_namespace.
SELECT 1 FROM pg_catalog.pg_namespace n
WHERE EXISTS (
  SELECT n.xmin FROM (SELECT 1 AS oid) n
);

-- CTE shadowing: same pattern via WITH clause.
WITH n AS (SELECT 1 AS oid)
SELECT 1 FROM pg_catalog.pg_namespace n2
WHERE EXISTS (
  SELECT n.xmin FROM n
);

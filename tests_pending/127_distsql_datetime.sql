-- PostgreSQL compatible tests from distsql_datetime
-- 1 tests

-- Setup: the upstream CockroachDB logic test creates and populates ts, then
-- uses range splits/relocation to force remote evaluation. PostgreSQL does not
-- have SPLIT/RELOCATE, but we keep the data setup and the final expression.
CREATE TABLE ts (a INT PRIMARY KEY, t TIMESTAMP);
INSERT INTO ts
SELECT i, TIMESTAMP '0001-01-01 00:00:00' + (i * INTERVAL '1 second')
FROM generate_series(1, 5) AS g(i);

-- ALTER TABLE ts SPLIT AT SELECT i FROM generate_series(2, 5) AS g(i);
-- ALTER TABLE ts EXPERIMENTAL_RELOCATE SELECT ARRAY[i%5+1], i FROM generate_series(1, 5) AS g(i);

-- Test 1: statement (line 14)
SELECT
    t
    - (
        SELECT TIMESTAMP '0001-01-01 00:00:00' - (a * INTERVAL '1 second')
        FROM ts
        ORDER BY a
        LIMIT 1
    )
FROM ts
ORDER BY a;

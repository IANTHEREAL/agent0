-- PostgreSQL compatible tests from distsql_datetime
-- 1 tests

-- Setup: the upstream CockroachDB logic test creates and populates ts, then
-- uses range splits/relocation to force remote evaluation. PostgreSQL does not
-- have SPLIT/RELOCATE, but we keep the data setup and the final expression.
DROP TABLE IF EXISTS ts;
CREATE TABLE ts (a INT PRIMARY KEY, t TIMESTAMP);
INSERT INTO ts
-- Use a timestamp range supported by tipg.
SELECT i, TIMESTAMP '2000-01-01 00:00:00' + ((i::text || ' seconds')::interval)
FROM generate_series(1, 5) AS g(i);

-- ALTER TABLE ts SPLIT AT SELECT i FROM generate_series(2, 5) AS g(i);
-- ALTER TABLE ts EXPERIMENTAL_RELOCATE SELECT ARRAY[i%5+1], i FROM generate_series(1, 5) AS g(i);

-- Test 1: statement (line 14)
SELECT
    EXTRACT(EPOCH FROM t)
    - (
        SELECT EXTRACT(
            EPOCH FROM TIMESTAMP '2000-01-01 00:00:00' - ((a::text || ' seconds')::interval)
        )
        FROM ts
        ORDER BY a
        LIMIT 1
    ) AS seconds
FROM ts
ORDER BY a;

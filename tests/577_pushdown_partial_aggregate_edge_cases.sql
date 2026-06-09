-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop surface: grouped aggregates stay local while preserving NULL group keys and nullable states.

DROP TABLE IF EXISTS agg_pushdown_edge;
DROP TABLE IF EXISTS agg_pushdown_edge_on;
DROP TABLE IF EXISTS agg_pushdown_edge_off;
DROP TABLE IF EXISTS agg_pushdown_edge_composite;
DROP TABLE IF EXISTS agg_pushdown_edge_composite_on;
DROP TABLE IF EXISTS agg_pushdown_edge_composite_off;

CREATE TABLE agg_pushdown_edge(
    id INT PRIMARY KEY,
    g INT,
    v INT,
    flag BOOLEAN
);

INSERT INTO agg_pushdown_edge VALUES
    (1, 1, 10, true),
    (2, 1, NULL, NULL),
    (3, NULL, NULL, NULL),
    (4, NULL, 20, false),
    (5, 2, NULL, NULL),
    (6, 2, NULL, NULL);
ANALYZE agg_pushdown_edge;

SET db9.enable_cop_pushdown = on;
SELECT 'aggregate_edges_local_on' AS phase;
\o /tmp/577_aggregate_edges_explain.txt
EXPLAIN
SELECT g, COUNT(v), SUM(v), BOOL_OR(flag), EVERY(flag), MIN(v), MAX(v)
FROM agg_pushdown_edge
GROUP BY g
ORDER BY g NULLS FIRST;
\o
\! cat /tmp/577_aggregate_edges_explain.txt
\! if grep -Fq "DB9 Cop Aggregate" /tmp/577_aggregate_edges_explain.txt; then echo "aggregate_edges_partial_pushdown|1"; else echo "aggregate_edges_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_edge_on AS
SELECT
    g,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge
GROUP BY g
ORDER BY g NULLS FIRST;

-- The aggregate contract under test is the materializing query above, not a
-- separate temp-table scan under cop pushdown.
SET db9.enable_cop_pushdown = off;

SELECT
    COALESCE(g::text, 'NULL'),
    cnt_v,
    COALESCE(sum_v::text, 'NULL'),
    COALESCE(any_true::text, 'NULL'),
    COALESCE(all_true::text, 'NULL'),
    COALESCE(min_v::text, 'NULL'),
    COALESCE(max_v::text, 'NULL')
FROM agg_pushdown_edge_on
ORDER BY g NULLS FIRST;

SELECT 'aggregate_edges_local_off' AS phase;

CREATE TEMP TABLE agg_pushdown_edge_off AS
SELECT
    g,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge
GROUP BY g
ORDER BY g NULLS FIRST;

SELECT 'aggregate_edge_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SET db9.enable_cop_pushdown = on;

SELECT 'aggregate_empty_local_on' AS phase;
\o /tmp/577_aggregate_empty_explain.txt
EXPLAIN
SELECT
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge
WHERE id < 0;
\o
\! cat /tmp/577_aggregate_empty_explain.txt
\! if grep -Fq "DB9 Cop Aggregate" /tmp/577_aggregate_empty_explain.txt; then echo "aggregate_empty_partial_pushdown|1"; else echo "aggregate_empty_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_edge_empty_on AS
SELECT
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge
WHERE id < 0;

SET db9.enable_cop_pushdown = off;

SELECT
    cnt_all,
    cnt_v,
    COALESCE(sum_v::text, 'NULL'),
    COALESCE(any_true::text, 'NULL'),
    COALESCE(all_true::text, 'NULL'),
    COALESCE(min_v::text, 'NULL'),
    COALESCE(max_v::text, 'NULL')
FROM agg_pushdown_edge_empty_on;

SELECT 'aggregate_empty_local_off' AS phase;

CREATE TEMP TABLE agg_pushdown_edge_empty_off AS
SELECT
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge
WHERE id < 0;

SELECT 'aggregate_empty_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_empty_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_empty_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_empty_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_empty_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE agg_pushdown_edge;
DROP TABLE agg_pushdown_edge_on;
DROP TABLE agg_pushdown_edge_off;
DROP TABLE agg_pushdown_edge_empty_on;
DROP TABLE agg_pushdown_edge_empty_off;

CREATE TABLE agg_pushdown_edge_composite(
    id INT PRIMARY KEY,
    g1 INT,
    g2 INT,
    v INT,
    flag BOOLEAN
);

INSERT INTO agg_pushdown_edge_composite VALUES
    (1, NULL, 1, 10, true),
    (2, NULL, 1, NULL, NULL),
    (3, NULL, 2, 20, false),
    (4, 1, NULL, 30, true),
    (5, 1, NULL, NULL, NULL),
    (6, 1, 2, NULL, NULL);
ANALYZE agg_pushdown_edge_composite;

SET db9.enable_cop_pushdown = on;

SELECT 'aggregate_composite_local_on' AS phase;
\o /tmp/577_aggregate_composite_explain.txt
EXPLAIN
SELECT g1, g2, COUNT(*), COUNT(v), SUM(v), BOOL_OR(flag), EVERY(flag), MIN(v), MAX(v)
FROM agg_pushdown_edge_composite
GROUP BY g1, g2
ORDER BY g1 NULLS FIRST, g2 NULLS FIRST;
\o
\! cat /tmp/577_aggregate_composite_explain.txt
\! if grep -Fq "DB9 Cop Aggregate" /tmp/577_aggregate_composite_explain.txt; then echo "aggregate_composite_partial_pushdown|1"; else echo "aggregate_composite_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_edge_composite_on AS
SELECT
    g1,
    g2,
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge_composite
GROUP BY g1, g2
ORDER BY g1 NULLS FIRST, g2 NULLS FIRST;

SET db9.enable_cop_pushdown = off;

SELECT
    COALESCE(g1::text, 'NULL'),
    COALESCE(g2::text, 'NULL'),
    cnt_all,
    cnt_v,
    COALESCE(sum_v::text, 'NULL'),
    COALESCE(any_true::text, 'NULL'),
    COALESCE(all_true::text, 'NULL'),
    COALESCE(min_v::text, 'NULL'),
    COALESCE(max_v::text, 'NULL')
FROM agg_pushdown_edge_composite_on
ORDER BY g1 NULLS FIRST, g2 NULLS FIRST;

SELECT 'aggregate_composite_local_off' AS phase;

CREATE TEMP TABLE agg_pushdown_edge_composite_off AS
SELECT
    g1,
    g2,
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    BOOL_OR(flag) AS any_true,
    EVERY(flag) AS all_true,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM agg_pushdown_edge_composite
GROUP BY g1, g2
ORDER BY g1 NULLS FIRST, g2 NULLS FIRST;

SELECT 'aggregate_composite_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_composite_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_composite_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_edge_composite_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_edge_composite_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE agg_pushdown_edge_composite;
\! rm -f /tmp/577_aggregate_edges_explain.txt /tmp/577_aggregate_empty_explain.txt /tmp/577_aggregate_composite_explain.txt
DROP TABLE agg_pushdown_edge_composite_on;
DROP TABLE agg_pushdown_edge_composite_off;

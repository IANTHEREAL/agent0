-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop surface: grouped aggregates stay local and preserve on/off parity.

DROP TABLE IF EXISTS agg_pushdown_demo;
DROP TABLE IF EXISTS agg_pushdown_demo_safe_on;
DROP TABLE IF EXISTS agg_pushdown_demo_safe_off;
DROP TABLE IF EXISTS agg_pushdown_demo_unsafe_on;
DROP TABLE IF EXISTS agg_pushdown_demo_unsafe_off;
DROP TABLE IF EXISTS agg_pushdown_demo_text_group_on;
DROP TABLE IF EXISTS agg_pushdown_demo_text_group_off;

CREATE TABLE agg_pushdown_demo(
    id INT PRIMARY KEY,
    g INT NOT NULL,
    v INT,
    flag BOOLEAN NOT NULL,
    label TEXT NOT NULL,
    f DOUBLE PRECISION NOT NULL
);

INSERT INTO agg_pushdown_demo VALUES
    (1, 1, 10, true,  'alpha', 1.5),
    (2, 1, 20, true,  'alpha', 2.25),
    (3, 2, 30, false, 'beta',  3.75),
    (4, 2, 40, true,  'beta',  4.5),
    (5, 3, NULL, true, 'gamma', 5.125),
    (6, 3, 50, false, 'gamma', 6.875);
ANALYZE agg_pushdown_demo;

SET db9.enable_cop_pushdown = on;
SELECT 'grouped_safe_local_on' AS phase;
\o /tmp/571_grouped_safe_explain.txt
EXPLAIN
SELECT g, COUNT(*), BOOL_AND(flag), MIN(v), MAX(v)
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY g
ORDER BY g;
\o
\! cat /tmp/571_grouped_safe_explain.txt
\! if grep -Fq "DB9 Cop Aggregate" /tmp/571_grouped_safe_explain.txt; then echo "grouped_safe_partial_pushdown|1"; else echo "grouped_safe_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_demo_safe_on AS
SELECT g, COUNT(*) AS cnt, BOOL_AND(flag) AS all_true, MIN(v) AS min_v, MAX(v) AS max_v
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY g
ORDER BY g;

SELECT g, cnt, all_true, min_v, max_v
FROM agg_pushdown_demo_safe_on;

\o /tmp/571_avg_float_explain.txt
EXPLAIN
SELECT AVG(f)
FROM agg_pushdown_demo
WHERE g >= 1;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/571_avg_float_explain.txt; then echo "avg_float_partial_pushdown|1"; else echo "avg_float_partial_pushdown|0"; fi

\o /tmp/571_sum_float_explain.txt
EXPLAIN
SELECT SUM(f)
FROM agg_pushdown_demo
WHERE g >= 1;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/571_sum_float_explain.txt; then echo "sum_float_partial_pushdown|1"; else echo "sum_float_partial_pushdown|0"; fi

\o /tmp/571_min_text_explain.txt
EXPLAIN
SELECT MIN(label)
FROM agg_pushdown_demo
WHERE g >= 1;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/571_min_text_explain.txt; then echo "min_text_partial_pushdown|1"; else echo "min_text_partial_pushdown|0"; fi

\o /tmp/571_text_group_key_explain.txt
EXPLAIN
SELECT label, COUNT(*)
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY label
ORDER BY label;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/571_text_group_key_explain.txt; then echo "text_group_key_partial_pushdown|1"; else echo "text_group_key_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_demo_unsafe_on AS
SELECT AVG(f) AS avg_f, SUM(f) AS sum_f, MIN(label) AS min_label
FROM agg_pushdown_demo
WHERE g >= 1;

CREATE TEMP TABLE agg_pushdown_demo_text_group_on AS
SELECT label, COUNT(*) AS cnt
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY label
ORDER BY label;

SET db9.enable_cop_pushdown = off;

SELECT 'grouped_safe_local_off' AS phase;
EXPLAIN
SELECT g, COUNT(*), BOOL_AND(flag), MIN(v), MAX(v)
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY g
ORDER BY g;

CREATE TEMP TABLE agg_pushdown_demo_safe_off AS
SELECT g, COUNT(*) AS cnt, BOOL_AND(flag) AS all_true, MIN(v) AS min_v, MAX(v) AS max_v
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY g
ORDER BY g;

CREATE TEMP TABLE agg_pushdown_demo_unsafe_off AS
SELECT AVG(f) AS avg_f, SUM(f) AS sum_f, MIN(label) AS min_label
FROM agg_pushdown_demo
WHERE g >= 1;

CREATE TEMP TABLE agg_pushdown_demo_text_group_off AS
SELECT label, COUNT(*) AS cnt
FROM agg_pushdown_demo
WHERE g >= 1
GROUP BY label
ORDER BY label;

SELECT 'grouped_safe_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_safe_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_safe_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_safe_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_safe_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'unsafe_aggregate_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_unsafe_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_unsafe_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_unsafe_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_unsafe_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'unsafe_text_group_key_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_text_group_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_text_group_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_demo_text_group_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_demo_text_group_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

\! rm -f /tmp/571_grouped_safe_explain.txt /tmp/571_avg_float_explain.txt /tmp/571_sum_float_explain.txt /tmp/571_min_text_explain.txt /tmp/571_text_group_key_explain.txt

DROP TABLE agg_pushdown_demo;
DROP TABLE agg_pushdown_demo_safe_on;
DROP TABLE agg_pushdown_demo_safe_off;
DROP TABLE agg_pushdown_demo_unsafe_on;
DROP TABLE agg_pushdown_demo_unsafe_off;
DROP TABLE agg_pushdown_demo_text_group_on;
DROP TABLE agg_pushdown_demo_text_group_off;

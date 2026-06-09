-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop surface: typed aggregate matrix stays local on the exact pair while preserving parity.

DROP TABLE IF EXISTS agg_pushdown_typed_matrix;
DROP TABLE IF EXISTS agg_pushdown_typed_bigint_on;
DROP TABLE IF EXISTS agg_pushdown_typed_bigint_off;
DROP TABLE IF EXISTS agg_pushdown_typed_bool_on;
DROP TABLE IF EXISTS agg_pushdown_typed_bool_off;
DROP TABLE IF EXISTS agg_pushdown_typed_date_on;
DROP TABLE IF EXISTS agg_pushdown_typed_date_off;
DROP TABLE IF EXISTS agg_pushdown_typed_time_on;
DROP TABLE IF EXISTS agg_pushdown_typed_time_off;
DROP TABLE IF EXISTS agg_pushdown_typed_ts_on;
DROP TABLE IF EXISTS agg_pushdown_typed_ts_off;
DROP TABLE IF EXISTS agg_pushdown_typed_tstz_on;
DROP TABLE IF EXISTS agg_pushdown_typed_tstz_off;

CREATE TABLE agg_pushdown_typed_matrix(
    id INT PRIMARY KEY,
    g_big BIGINT NOT NULL,
    g_bool BOOLEAN NOT NULL,
    g_date DATE NOT NULL,
    g_time TIME NOT NULL,
    g_ts TIMESTAMP NOT NULL,
    g_tstz TIMESTAMPTZ NOT NULL,
    v_big BIGINT,
    v_bool BOOLEAN,
    v_date DATE,
    v_time TIME,
    v_ts TIMESTAMP,
    v_tstz TIMESTAMPTZ
);

INSERT INTO agg_pushdown_typed_matrix VALUES
    (
        1,
        10000000000,
        true,
        DATE '2024-01-10',
        TIME '08:00:00',
        TIMESTAMP '2024-01-10 08:00:00',
        TIMESTAMPTZ '2024-01-10 08:00:00+00',
        10000000000,
        true,
        DATE '2024-01-01',
        TIME '07:00:00',
        TIMESTAMP '2024-01-10 07:00:00',
        TIMESTAMPTZ '2024-01-10 07:00:00+00'
    ),
    (
        2,
        10000000000,
        true,
        DATE '2024-01-10',
        TIME '08:00:00',
        TIMESTAMP '2024-01-10 08:00:00',
        TIMESTAMPTZ '2024-01-10 08:00:00+00',
        20000000000,
        false,
        DATE '2024-01-05',
        TIME '08:30:00',
        TIMESTAMP '2024-01-10 08:30:00',
        TIMESTAMPTZ '2024-01-10 08:30:00+00'
    ),
    (
        3,
        20000000000,
        false,
        DATE '2024-01-20',
        TIME '09:30:00',
        TIMESTAMP '2024-01-20 09:30:00',
        TIMESTAMPTZ '2024-01-20 09:30:00+00',
        30000000000,
        false,
        DATE '2024-01-11',
        TIME '09:00:00',
        TIMESTAMP '2024-01-20 09:00:00',
        TIMESTAMPTZ '2024-01-20 09:00:00+00'
    ),
    (
        4,
        20000000000,
        false,
        DATE '2024-01-20',
        TIME '09:30:00',
        TIMESTAMP '2024-01-20 09:30:00',
        TIMESTAMPTZ '2024-01-20 09:30:00+00',
        40000000000,
        false,
        DATE '2024-01-15',
        TIME '10:45:00',
        TIMESTAMP '2024-01-20 10:45:00',
        TIMESTAMPTZ '2024-01-20 10:45:00+00'
    );
ANALYZE agg_pushdown_typed_matrix;

SET TIME ZONE 'UTC';
SET db9.enable_cop_pushdown = on;
\o /tmp/579_typed_bigint_explain.txt
EXPLAIN
SELECT g_big, SUM(v_big), MIN(v_big), MAX(v_big)
FROM agg_pushdown_typed_matrix
GROUP BY g_big
ORDER BY g_big;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_bigint_explain.txt; then echo "typed_bigint_partial_pushdown|1"; else echo "typed_bigint_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_bigint_on AS
SELECT
    g_big,
    SUM(v_big) AS sum_v_big,
    MIN(v_big) AS min_v_big,
    MAX(v_big) AS max_v_big
FROM agg_pushdown_typed_matrix
GROUP BY g_big
ORDER BY g_big;

SELECT g_big::text, sum_v_big::text, min_v_big::text, max_v_big::text
FROM agg_pushdown_typed_bigint_on
ORDER BY g_big;

\o /tmp/579_typed_bool_explain.txt
EXPLAIN
SELECT g_bool, MIN(v_bool), MAX(v_bool)
FROM agg_pushdown_typed_matrix
GROUP BY g_bool
ORDER BY g_bool;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_bool_explain.txt; then echo "typed_bool_partial_pushdown|1"; else echo "typed_bool_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_bool_on AS
SELECT
    g_bool,
    MIN(v_bool) AS min_v_bool,
    MAX(v_bool) AS max_v_bool
FROM agg_pushdown_typed_matrix
GROUP BY g_bool
ORDER BY g_bool;

SELECT g_bool::text, min_v_bool::text, max_v_bool::text
FROM agg_pushdown_typed_bool_on
ORDER BY g_bool;

\o /tmp/579_typed_date_explain.txt
EXPLAIN
SELECT g_date, MIN(v_date), MAX(v_date)
FROM agg_pushdown_typed_matrix
GROUP BY g_date
ORDER BY g_date;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_date_explain.txt; then echo "typed_date_partial_pushdown|1"; else echo "typed_date_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_date_on AS
SELECT
    g_date,
    MIN(v_date) AS min_v_date,
    MAX(v_date) AS max_v_date
FROM agg_pushdown_typed_matrix
GROUP BY g_date
ORDER BY g_date;

SELECT g_date::text, min_v_date::text, max_v_date::text
FROM agg_pushdown_typed_date_on
ORDER BY g_date;

\o /tmp/579_typed_time_explain.txt
EXPLAIN
SELECT g_time, MIN(v_time), MAX(v_time)
FROM agg_pushdown_typed_matrix
GROUP BY g_time
ORDER BY g_time;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_time_explain.txt; then echo "typed_time_partial_pushdown|1"; else echo "typed_time_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_time_on AS
SELECT
    g_time,
    MIN(v_time) AS min_v_time,
    MAX(v_time) AS max_v_time
FROM agg_pushdown_typed_matrix
GROUP BY g_time
ORDER BY g_time;

SELECT g_time::text, min_v_time::text, max_v_time::text
FROM agg_pushdown_typed_time_on
ORDER BY g_time;

\o /tmp/579_typed_ts_explain.txt
EXPLAIN
SELECT g_ts, MIN(v_ts), MAX(v_ts)
FROM agg_pushdown_typed_matrix
GROUP BY g_ts
ORDER BY g_ts;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_ts_explain.txt; then echo "typed_timestamp_partial_pushdown|1"; else echo "typed_timestamp_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_ts_on AS
SELECT
    g_ts,
    MIN(v_ts) AS min_v_ts,
    MAX(v_ts) AS max_v_ts
FROM agg_pushdown_typed_matrix
GROUP BY g_ts
ORDER BY g_ts;

SELECT g_ts::text, min_v_ts::text, max_v_ts::text
FROM agg_pushdown_typed_ts_on
ORDER BY g_ts;

\o /tmp/579_typed_tstz_explain.txt
EXPLAIN
SELECT g_tstz, MIN(v_tstz), MAX(v_tstz)
FROM agg_pushdown_typed_matrix
GROUP BY g_tstz
ORDER BY g_tstz;
\o
\! if grep -Fq "DB9 Cop Aggregate" /tmp/579_typed_tstz_explain.txt; then echo "typed_timestamptz_partial_pushdown|1"; else echo "typed_timestamptz_partial_pushdown|0"; fi

CREATE TEMP TABLE agg_pushdown_typed_tstz_on AS
SELECT
    g_tstz,
    MIN(v_tstz) AS min_v_tstz,
    MAX(v_tstz) AS max_v_tstz
FROM agg_pushdown_typed_matrix
GROUP BY g_tstz
ORDER BY g_tstz;

SELECT g_tstz::text, min_v_tstz::text, max_v_tstz::text
FROM agg_pushdown_typed_tstz_on
ORDER BY g_tstz;

SET db9.enable_cop_pushdown = off;

CREATE TEMP TABLE agg_pushdown_typed_bigint_off AS
SELECT
    g_big,
    SUM(v_big) AS sum_v_big,
    MIN(v_big) AS min_v_big,
    MAX(v_big) AS max_v_big
FROM agg_pushdown_typed_matrix
GROUP BY g_big
ORDER BY g_big;

CREATE TEMP TABLE agg_pushdown_typed_bool_off AS
SELECT
    g_bool,
    MIN(v_bool) AS min_v_bool,
    MAX(v_bool) AS max_v_bool
FROM agg_pushdown_typed_matrix
GROUP BY g_bool
ORDER BY g_bool;

CREATE TEMP TABLE agg_pushdown_typed_date_off AS
SELECT
    g_date,
    MIN(v_date) AS min_v_date,
    MAX(v_date) AS max_v_date
FROM agg_pushdown_typed_matrix
GROUP BY g_date
ORDER BY g_date;

CREATE TEMP TABLE agg_pushdown_typed_time_off AS
SELECT
    g_time,
    MIN(v_time) AS min_v_time,
    MAX(v_time) AS max_v_time
FROM agg_pushdown_typed_matrix
GROUP BY g_time
ORDER BY g_time;

CREATE TEMP TABLE agg_pushdown_typed_ts_off AS
SELECT
    g_ts,
    MIN(v_ts) AS min_v_ts,
    MAX(v_ts) AS max_v_ts
FROM agg_pushdown_typed_matrix
GROUP BY g_ts
ORDER BY g_ts;

CREATE TEMP TABLE agg_pushdown_typed_tstz_off AS
SELECT
    g_tstz,
    MIN(v_tstz) AS min_v_tstz,
    MAX(v_tstz) AS max_v_tstz
FROM agg_pushdown_typed_matrix
GROUP BY g_tstz
ORDER BY g_tstz;

SELECT 'typed_bigint_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_bigint_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_bigint_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_bigint_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_bigint_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'typed_bool_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_bool_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_bool_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_bool_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_bool_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'typed_date_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_date_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_date_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_date_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_date_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'typed_time_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_time_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_time_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_time_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_time_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'typed_timestamp_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_ts_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_ts_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_ts_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_ts_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'typed_timestamptz_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_tstz_on
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_tstz_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM agg_pushdown_typed_tstz_off
            EXCEPT ALL
            SELECT * FROM agg_pushdown_typed_tstz_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

\! rm -f /tmp/579_typed_bigint_explain.txt /tmp/579_typed_bool_explain.txt /tmp/579_typed_date_explain.txt /tmp/579_typed_time_explain.txt /tmp/579_typed_ts_explain.txt /tmp/579_typed_tstz_explain.txt

DROP TABLE agg_pushdown_typed_matrix;
DROP TABLE agg_pushdown_typed_bigint_on;
DROP TABLE agg_pushdown_typed_bigint_off;
DROP TABLE agg_pushdown_typed_bool_on;
DROP TABLE agg_pushdown_typed_bool_off;
DROP TABLE agg_pushdown_typed_date_on;
DROP TABLE agg_pushdown_typed_date_off;
DROP TABLE agg_pushdown_typed_time_on;
DROP TABLE agg_pushdown_typed_time_off;
DROP TABLE agg_pushdown_typed_ts_on;
DROP TABLE agg_pushdown_typed_ts_off;
DROP TABLE agg_pushdown_typed_tstz_on;
DROP TABLE agg_pushdown_typed_tstz_off;

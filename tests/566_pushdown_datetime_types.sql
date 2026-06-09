-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: timestamp index range, timestamptz point lookup, and datetime fallback parity.

DROP TABLE IF EXISTS db9_cop_datetime_smoke;
DROP TABLE IF EXISTS db9_cop_datetime_range_on;
DROP TABLE IF EXISTS db9_cop_datetime_range_off;
DROP TABLE IF EXISTS db9_cop_datetime_tz_on;
DROP TABLE IF EXISTS db9_cop_datetime_tz_off;
DROP TABLE IF EXISTS db9_cop_datetime_part_on;
DROP TABLE IF EXISTS db9_cop_datetime_part_off;
DROP TABLE IF EXISTS db9_cop_datetime_trunc_on;
DROP TABLE IF EXISTS db9_cop_datetime_trunc_off;

CREATE TABLE db9_cop_datetime_smoke(
    id INT PRIMARY KEY,
    created_at TIMESTAMP,
    created_tz TIMESTAMPTZ,
    note TEXT
);
CREATE INDEX db9_cop_datetime_smoke_created_at_idx ON db9_cop_datetime_smoke(created_at);
CREATE INDEX db9_cop_datetime_smoke_created_tz_idx ON db9_cop_datetime_smoke(created_tz);
INSERT INTO db9_cop_datetime_smoke VALUES
    (1, '2024-01-01 00:00:00', '2024-01-01 00:00:00+00', 'a'),
    (2, '2024-01-02 12:34:56', '2024-01-02 20:34:56+08', 'b'),
    (3, '2024-01-03 23:59:59', '2024-01-03 23:59:59+00', 'c'),
    (4, '2024-01-04 08:00:00', '2024-01-04 16:00:00+08', 'd'),
    (5, to_timestamp('Infinity'::double precision), to_timestamp('Infinity'::double precision), 'inf'),
    (6, to_timestamp('-Infinity'::double precision), to_timestamp('-Infinity'::double precision), 'neg_inf');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, created_at
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-04 00:00:00'
LIMIT 2;
SELECT id, created_at
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-04 00:00:00'
LIMIT 2;
CREATE TEMP TABLE db9_cop_datetime_range_on AS
SELECT id, created_at
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-04 00:00:00'
LIMIT 2;

EXPLAIN SELECT id, created_tz
FROM db9_cop_datetime_smoke
WHERE created_tz = TIMESTAMPTZ '2024-01-04 08:00:00+00'
LIMIT 1;
SELECT id, created_tz
FROM db9_cop_datetime_smoke
WHERE created_tz = TIMESTAMPTZ '2024-01-04 08:00:00+00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_tz_on AS
SELECT id, created_tz
FROM db9_cop_datetime_smoke
WHERE created_tz = TIMESTAMPTZ '2024-01-04 08:00:00+00'
LIMIT 1;

\o /tmp/566_date_trunc_timestamp_explain.txt
EXPLAIN SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Filter: (date_trunc(" /tmp/566_date_trunc_timestamp_explain.txt; then echo "date_trunc_timestamp_pushes|1"; else echo "date_trunc_timestamp_pushes|0"; fi
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_trunc_on AS
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;

\o /tmp/566_date_part_trunc_timestamp_explain.txt
EXPLAIN SELECT id, date_part('day', created_at), date_trunc('hour', created_at)
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-03 00:00:00'
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output: id, date_part, date_trunc" /tmp/566_date_part_trunc_timestamp_explain.txt; then echo "date_part_trunc_timestamp_pushes|1"; else echo "date_part_trunc_timestamp_pushes|0"; fi
SELECT id, date_part('day', created_at), date_trunc('hour', created_at)
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-03 00:00:00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_part_on AS
SELECT id, date_part('day', created_at), date_trunc('hour', created_at)
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-03 00:00:00'
LIMIT 1;

\o /tmp/566_date_part_infinity_explain.txt
EXPLAIN SELECT id,
       extract(year FROM created_at),
       extract(epoch FROM created_at),
       date_part('year', created_at),
       date_part('month', created_at),
       date_trunc('day', created_at),
       to_char(created_at, 'YYYY'),
       date(created_at)
FROM db9_cop_datetime_smoke
WHERE id IN (5, 6)
ORDER BY id;
\o
\! if grep -Fq "DB9 Cop Output: id, extract, extract, date_part, date_part, date_trunc, to_char, date" /tmp/566_date_part_infinity_explain.txt; then echo "date_part_infinity_timestamp_pushes|1"; else echo "date_part_infinity_timestamp_pushes|0"; fi
SELECT id,
       extract(year FROM created_at),
       extract(epoch FROM created_at),
       date_part('year', created_at),
       date_part('month', created_at),
       date_trunc('day', created_at),
       to_char(created_at, 'YYYY'),
       date(created_at)
FROM db9_cop_datetime_smoke
WHERE id IN (5, 6)
ORDER BY id;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_datetime_range_off AS
SELECT id, created_at
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-04 00:00:00'
LIMIT 2;
CREATE TEMP TABLE db9_cop_datetime_tz_off AS
SELECT id, created_tz
FROM db9_cop_datetime_smoke
WHERE created_tz = TIMESTAMPTZ '2024-01-04 08:00:00+00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_part_off AS
SELECT id, date_part('day', created_at), date_trunc('hour', created_at)
FROM db9_cop_datetime_smoke
WHERE created_at >= TIMESTAMP '2024-01-02 00:00:00'
  AND created_at < TIMESTAMP '2024-01-03 00:00:00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_trunc_off AS
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;
SELECT id,
       extract(year FROM created_at),
       extract(epoch FROM created_at),
       date_part('year', created_at),
       date_part('month', created_at),
       date_trunc('day', created_at),
       to_char(created_at, 'YYYY'),
       date(created_at)
FROM db9_cop_datetime_smoke
WHERE id IN (5, 6)
ORDER BY id;

SELECT 'datetime_range_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_range_on
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_range_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_range_off
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_range_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'datetime_timestamptz_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_tz_on
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_tz_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_tz_off
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_tz_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'datetime_date_trunc_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_trunc_on
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_trunc_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_trunc_off
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_trunc_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'datetime_part_trunc_projection_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_part_on
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_part_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_datetime_part_off
            EXCEPT ALL
            SELECT * FROM db9_cop_datetime_part_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

\! rm -f /tmp/566_date_trunc_timestamp_explain.txt /tmp/566_date_part_trunc_timestamp_explain.txt /tmp/566_date_part_infinity_explain.txt

DROP TABLE db9_cop_datetime_smoke;
DROP TABLE db9_cop_datetime_range_on;
DROP TABLE db9_cop_datetime_range_off;
DROP TABLE db9_cop_datetime_tz_on;
DROP TABLE db9_cop_datetime_tz_off;
DROP TABLE db9_cop_datetime_part_on;
DROP TABLE db9_cop_datetime_part_off;
DROP TABLE db9_cop_datetime_trunc_on;
DROP TABLE db9_cop_datetime_trunc_off;

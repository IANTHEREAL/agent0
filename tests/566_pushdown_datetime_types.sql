-- DB9 cop pushdown: timestamp index range, timestamptz point lookup, and datetime fallback parity.

DROP TABLE IF EXISTS db9_cop_datetime_smoke;
DROP TABLE IF EXISTS db9_cop_datetime_range_on;
DROP TABLE IF EXISTS db9_cop_datetime_range_off;
DROP TABLE IF EXISTS db9_cop_datetime_tz_on;
DROP TABLE IF EXISTS db9_cop_datetime_tz_off;
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
    (4, '2024-01-04 08:00:00', '2024-01-04 16:00:00+08', 'd');

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

EXPLAIN SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;
CREATE TEMP TABLE db9_cop_datetime_trunc_on AS
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;

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
CREATE TEMP TABLE db9_cop_datetime_trunc_off AS
SELECT id
FROM db9_cop_datetime_smoke
WHERE date_trunc('day', created_at) = TIMESTAMP '2024-01-02 00:00:00'
LIMIT 1;

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

DROP TABLE db9_cop_datetime_smoke;
DROP TABLE db9_cop_datetime_range_on;
DROP TABLE db9_cop_datetime_range_off;
DROP TABLE db9_cop_datetime_tz_on;
DROP TABLE db9_cop_datetime_tz_off;
DROP TABLE db9_cop_datetime_trunc_on;
DROP TABLE db9_cop_datetime_trunc_off;

-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: timestamptz range pushdown and AT TIME ZONE fallback parity.

DROP TABLE IF EXISTS db9_cop_timestamptz_smoke;
DROP TABLE IF EXISTS db9_cop_timestamptz_range_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_range_off;
DROP TABLE IF EXISTS db9_cop_timestamptz_zone_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_zone_off;
DROP TABLE IF EXISTS db9_cop_timestamptz_date_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_date_off;
DROP TABLE IF EXISTS db9_cop_timestamptz_age_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_age_off;

CREATE TABLE db9_cop_timestamptz_smoke(
    id INT PRIMARY KEY,
    created_tz TIMESTAMPTZ,
    note TEXT
);
CREATE INDEX db9_cop_timestamptz_smoke_created_tz_idx ON db9_cop_timestamptz_smoke(created_tz);
INSERT INTO db9_cop_timestamptz_smoke VALUES
    (1, '2024-01-01 00:00:00+00', 'a'),
    (2, '2024-01-02 20:34:56+08', 'b'),
    (3, '2024-01-03 23:59:59+00', 'c'),
    (4, '2024-01-04 16:00:00+08', 'd'),
    (5, '2024-01-01 23:30:00+00', 'boundary'),
    (6, '2024-03-01 00:30:00+08', 'age-boundary');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, created_tz
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-02 00:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-04 00:00:00+00'
LIMIT 2;
SELECT id, created_tz
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-02 00:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-04 00:00:00+00'
LIMIT 2;
CREATE TEMP TABLE db9_cop_timestamptz_range_on AS
SELECT id, created_tz
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-02 00:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-04 00:00:00+00'
LIMIT 2;

EXPLAIN SELECT id
FROM db9_cop_timestamptz_smoke
WHERE (created_tz AT TIME ZONE 'UTC') >= TIMESTAMP '2024-01-02 00:00:00'
LIMIT 2;
SELECT id
FROM db9_cop_timestamptz_smoke
WHERE (created_tz AT TIME ZONE 'UTC') >= TIMESTAMP '2024-01-02 00:00:00'
LIMIT 2;
CREATE TEMP TABLE db9_cop_timestamptz_zone_on AS
SELECT id
FROM db9_cop_timestamptz_smoke
WHERE (created_tz AT TIME ZONE 'UTC') >= TIMESTAMP '2024-01-02 00:00:00'
LIMIT 2;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_timestamptz_range_off AS
SELECT id, created_tz
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-02 00:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-04 00:00:00+00'
LIMIT 2;
CREATE TEMP TABLE db9_cop_timestamptz_zone_off AS
SELECT id
FROM db9_cop_timestamptz_smoke
WHERE (created_tz AT TIME ZONE 'UTC') >= TIMESTAMP '2024-01-02 00:00:00'
LIMIT 2;

SET timezone = 'Asia/Shanghai';

SET db9.enable_cop_pushdown = on;
\o /tmp/567_date_timestamptz_explain.txt
EXPLAIN SELECT DATE(created_tz) AS created_date
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-01 23:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-02 00:00:00+00';
\o
\! if grep -Fq "DB9 Cop Access:" /tmp/567_date_timestamptz_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/567_date_timestamptz_explain.txt; then echo "date_timestamptz_stays_local|1"; else echo "date_timestamptz_stays_local|0"; fi

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_timestamptz_date_on AS
SELECT DATE(created_tz) AS created_date
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-01 23:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-02 00:00:00+00';

SELECT created_date
FROM db9_cop_timestamptz_date_on;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_timestamptz_date_off AS
SELECT DATE(created_tz) AS created_date
FROM db9_cop_timestamptz_smoke
WHERE created_tz >= TIMESTAMPTZ '2024-01-01 23:00:00+00'
  AND created_tz < TIMESTAMPTZ '2024-01-02 00:00:00+00';

SELECT 'date_timestamptz_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_date_on
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_date_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_date_off
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_date_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SET db9.enable_cop_pushdown = on;
\o /tmp/567_age_timestamptz_explain.txt
EXPLAIN SELECT AGE(created_tz, TIMESTAMPTZ '2024-02-01 00:30:00+08') AS delta
FROM db9_cop_timestamptz_smoke
WHERE created_tz = TIMESTAMPTZ '2024-03-01 00:30:00+08';
\o
\! if grep -Fq "DB9 Cop Access:" /tmp/567_age_timestamptz_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/567_age_timestamptz_explain.txt; then echo "age_timestamptz_stays_local|1"; else echo "age_timestamptz_stays_local|0"; fi

\o /tmp/567_age_timestamptz_nested_explain.txt
EXPLAIN SELECT DATE_PART('epoch', AGE(created_tz, TIMESTAMPTZ '2024-02-01 00:30:00+08')) AS delta_seconds
FROM db9_cop_timestamptz_smoke
WHERE created_tz = TIMESTAMPTZ '2024-03-01 00:30:00+08';
\o
\! if grep -Fq "DB9 Cop Access:" /tmp/567_age_timestamptz_nested_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/567_age_timestamptz_nested_explain.txt; then echo "age_timestamptz_nested_stays_local|1"; else echo "age_timestamptz_nested_stays_local|0"; fi

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_timestamptz_age_on AS
SELECT AGE(created_tz, TIMESTAMPTZ '2024-02-01 00:30:00+08') AS delta
FROM db9_cop_timestamptz_smoke
WHERE created_tz = TIMESTAMPTZ '2024-03-01 00:30:00+08';

SELECT delta
FROM db9_cop_timestamptz_age_on;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_timestamptz_age_off AS
SELECT AGE(created_tz, TIMESTAMPTZ '2024-02-01 00:30:00+08') AS delta
FROM db9_cop_timestamptz_smoke
WHERE created_tz = TIMESTAMPTZ '2024-03-01 00:30:00+08';

SELECT 'age_timestamptz_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_age_on
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_age_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_age_off
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_age_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

RESET timezone;

\! rm -f /tmp/567_date_timestamptz_explain.txt /tmp/567_age_timestamptz_explain.txt /tmp/567_age_timestamptz_nested_explain.txt

SELECT 'timestamptz_range_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_range_on
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_range_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_range_off
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_range_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'timestamptz_at_time_zone_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_zone_on
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_zone_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timestamptz_zone_off
            EXCEPT ALL
            SELECT * FROM db9_cop_timestamptz_zone_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_timestamptz_smoke;
DROP TABLE db9_cop_timestamptz_range_on;
DROP TABLE db9_cop_timestamptz_range_off;
DROP TABLE db9_cop_timestamptz_zone_on;
DROP TABLE db9_cop_timestamptz_zone_off;
DROP TABLE db9_cop_timestamptz_date_on;
DROP TABLE db9_cop_timestamptz_date_off;
DROP TABLE db9_cop_timestamptz_age_on;
DROP TABLE db9_cop_timestamptz_age_off;

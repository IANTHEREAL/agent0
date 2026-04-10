-- DB9 cop pushdown: timestamptz range pushdown and AT TIME ZONE fallback parity.

DROP TABLE IF EXISTS db9_cop_timestamptz_smoke;
DROP TABLE IF EXISTS db9_cop_timestamptz_range_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_range_off;
DROP TABLE IF EXISTS db9_cop_timestamptz_zone_on;
DROP TABLE IF EXISTS db9_cop_timestamptz_zone_off;

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
    (4, '2024-01-04 16:00:00+08', 'd');

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

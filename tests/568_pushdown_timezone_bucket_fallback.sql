-- DB9 cop pushdown: timezone day-bucketing expression keeps local semantics under pushdown-on.

DROP TABLE IF EXISTS db9_cop_timezone_bucket_smoke;
DROP TABLE IF EXISTS db9_cop_timezone_bucket_on;
DROP TABLE IF EXISTS db9_cop_timezone_bucket_off;

CREATE TABLE db9_cop_timezone_bucket_smoke(
    id INT PRIMARY KEY,
    created_at TIMESTAMP,
    note TEXT
);
INSERT INTO db9_cop_timezone_bucket_smoke VALUES
    (1, '2024-01-02 15:30:00', 'a'),
    (2, '2024-01-02 16:30:00', 'b'),
    (3, '2024-01-03 00:15:00', 'c');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Shanghai')) AS bucket
FROM db9_cop_timezone_bucket_smoke
WHERE id <= 3
LIMIT 3;
SELECT id, DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Shanghai')) AS bucket
FROM db9_cop_timezone_bucket_smoke
WHERE id <= 3
ORDER BY id
LIMIT 3;
CREATE TEMP TABLE db9_cop_timezone_bucket_on AS
SELECT id, DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Shanghai')) AS bucket
FROM db9_cop_timezone_bucket_smoke
WHERE id <= 3
ORDER BY id
LIMIT 3;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_timezone_bucket_off AS
SELECT id, DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Shanghai')) AS bucket
FROM db9_cop_timezone_bucket_smoke
WHERE id <= 3
ORDER BY id
LIMIT 3;

SELECT 'timezone_bucket_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timezone_bucket_on
            EXCEPT ALL
            SELECT * FROM db9_cop_timezone_bucket_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_timezone_bucket_off
            EXCEPT ALL
            SELECT * FROM db9_cop_timezone_bucket_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_timezone_bucket_smoke;
DROP TABLE db9_cop_timezone_bucket_on;
DROP TABLE db9_cop_timezone_bucket_off;

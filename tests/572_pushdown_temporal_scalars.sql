-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: deterministic temporal scalar functions stay local while preserving parity.

DROP TABLE IF EXISTS db9_cop_temporal_scalar_smoke;
DROP TABLE IF EXISTS db9_cop_temporal_scalar_on;
DROP TABLE IF EXISTS db9_cop_temporal_scalar_off;

CREATE TABLE db9_cop_temporal_scalar_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    created_at TIMESTAMP NOT NULL
);
CREATE INDEX db9_cop_temporal_scalar_smoke_n_idx ON db9_cop_temporal_scalar_smoke(n);
INSERT INTO db9_cop_temporal_scalar_smoke VALUES
    (1, 10, '2024-01-01 00:00:00'),
    (2, 20, '2024-01-02 12:34:56'),
    (3, 30, '2024-01-03 23:59:59');

SET db9.enable_cop_pushdown = on;
\o /tmp/572_temporal_scalar_explain.txt
EXPLAIN SELECT
    DATE(created_at) AS created_date,
    MAKE_DATE(2024, 1, 2) AS fixed_date,
    MAKE_TIME(8, 15, 23) AS fixed_time,
    MAKE_TIMESTAMP(2024, 1, 2, 8, 15, 23) AS fixed_ts,
    TO_TIMESTAMP(1704183323) AS fixed_tz,
    TO_CHAR(created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_label,
    TO_CHAR(DATE(created_at), 'YYYY-MM-DD') AS created_day
FROM db9_cop_temporal_scalar_smoke
WHERE n = 20
LIMIT 1;
\o
\! if ! grep -Fq "DB9 Cop Output:" /tmp/572_temporal_scalar_explain.txt; then echo "temporal_scalar_projection_stays_local|1"; else echo "temporal_scalar_projection_stays_local|0"; fi
SELECT
    DATE(created_at) AS created_date,
    MAKE_DATE(2024, 1, 2) AS fixed_date,
    MAKE_TIME(8, 15, 23) AS fixed_time,
    MAKE_TIMESTAMP(2024, 1, 2, 8, 15, 23) AS fixed_ts,
    TO_TIMESTAMP(1704183323) AS fixed_tz,
    TO_CHAR(created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_label,
    TO_CHAR(DATE(created_at), 'YYYY-MM-DD') AS created_day
FROM db9_cop_temporal_scalar_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_temporal_scalar_on AS
SELECT
    DATE(created_at) AS created_date,
    MAKE_DATE(2024, 1, 2) AS fixed_date,
    MAKE_TIME(8, 15, 23) AS fixed_time,
    MAKE_TIMESTAMP(2024, 1, 2, 8, 15, 23) AS fixed_ts,
    TO_TIMESTAMP(1704183323) AS fixed_tz,
    TO_CHAR(created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_label,
    TO_CHAR(DATE(created_at), 'YYYY-MM-DD') AS created_day
FROM db9_cop_temporal_scalar_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_temporal_scalar_off AS
SELECT
    DATE(created_at) AS created_date,
    MAKE_DATE(2024, 1, 2) AS fixed_date,
    MAKE_TIME(8, 15, 23) AS fixed_time,
    MAKE_TIMESTAMP(2024, 1, 2, 8, 15, 23) AS fixed_ts,
    TO_TIMESTAMP(1704183323) AS fixed_tz,
    TO_CHAR(created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_label,
    TO_CHAR(DATE(created_at), 'YYYY-MM-DD') AS created_day
FROM db9_cop_temporal_scalar_smoke
WHERE n = 20
LIMIT 1;

SELECT 'temporal_scalar_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_temporal_scalar_on
            EXCEPT ALL
            SELECT * FROM db9_cop_temporal_scalar_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_temporal_scalar_off
            EXCEPT ALL
            SELECT * FROM db9_cop_temporal_scalar_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

\! rm -f /tmp/572_temporal_scalar_explain.txt

DROP TABLE db9_cop_temporal_scalar_smoke;
DROP TABLE db9_cop_temporal_scalar_on;
DROP TABLE db9_cop_temporal_scalar_off;

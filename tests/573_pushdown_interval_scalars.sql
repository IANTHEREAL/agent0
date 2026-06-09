-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: scalar AGE() interval outputs push down while unsupported interval
-- constructors stay local and preserve parity.

DROP TABLE IF EXISTS db9_cop_interval_scalar_smoke;
DROP TABLE IF EXISTS db9_cop_interval_scalar_on;
DROP TABLE IF EXISTS db9_cop_interval_scalar_off;

CREATE TABLE db9_cop_interval_scalar_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    created_at TIMESTAMP NOT NULL
);
CREATE INDEX db9_cop_interval_scalar_smoke_n_idx ON db9_cop_interval_scalar_smoke(n);
INSERT INTO db9_cop_interval_scalar_smoke VALUES
    (1, 10, '2024-01-01 00:00:00'),
    (2, 20, '2024-03-15 12:34:56'),
    (3, 30, '2024-05-01 08:00:00');

SET db9.enable_cop_pushdown = on;
\o /tmp/573_age_interval_explain.txt
EXPLAIN SELECT
    AGE(created_at, TIMESTAMP '2024-01-02 00:00:00') AS elapsed
FROM db9_cop_interval_scalar_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output: elapsed" /tmp/573_age_interval_explain.txt; then echo "age_interval_projection_pushes|1"; else echo "age_interval_projection_pushes|0"; fi

\o /tmp/573_interval_scalar_explain.txt
EXPLAIN SELECT
    AGE(created_at, TIMESTAMP '2024-01-02 00:00:00') AS elapsed,
    MAKE_INTERVAL(1, 2, 0, 3, 4, 5, 6) AS fixed_interval
FROM db9_cop_interval_scalar_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access:" /tmp/573_interval_scalar_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/573_interval_scalar_explain.txt; then echo "make_interval_projection_stays_local|1"; else echo "make_interval_projection_stays_local|0"; fi
SELECT
    AGE(created_at, TIMESTAMP '2024-01-02 00:00:00') AS elapsed,
    MAKE_INTERVAL(1, 2, 0, 3, 4, 5, 6) AS fixed_interval
FROM db9_cop_interval_scalar_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_interval_scalar_on AS
SELECT
    AGE(created_at, TIMESTAMP '2024-01-02 00:00:00') AS elapsed,
    MAKE_INTERVAL(1, 2, 0, 3, 4, 5, 6) AS fixed_interval
FROM db9_cop_interval_scalar_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_interval_scalar_off AS
SELECT
    AGE(created_at, TIMESTAMP '2024-01-02 00:00:00') AS elapsed,
    MAKE_INTERVAL(1, 2, 0, 3, 4, 5, 6) AS fixed_interval
FROM db9_cop_interval_scalar_smoke
WHERE n = 20
LIMIT 1;

SELECT 'interval_scalar_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_interval_scalar_on
            EXCEPT ALL
            SELECT * FROM db9_cop_interval_scalar_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_interval_scalar_off
            EXCEPT ALL
            SELECT * FROM db9_cop_interval_scalar_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

\! rm -f /tmp/573_age_interval_explain.txt /tmp/573_interval_scalar_explain.txt

DROP TABLE db9_cop_interval_scalar_smoke;
DROP TABLE db9_cop_interval_scalar_on;
DROP TABLE db9_cop_interval_scalar_off;

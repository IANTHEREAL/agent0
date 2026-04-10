-- DB9 cop pushdown: composite-index prefix and bounded-prefix access expose detail lines.

DROP TABLE IF EXISTS db9_cop_prefix_smoke;
DROP TABLE IF EXISTS db9_cop_prefix_on;
DROP TABLE IF EXISTS db9_cop_prefix_off;
DROP TABLE IF EXISTS db9_cop_prefix_range_on;
DROP TABLE IF EXISTS db9_cop_prefix_range_off;
CREATE TABLE db9_cop_prefix_smoke(id INT PRIMARY KEY, status TEXT, created_at INT, note TEXT);
CREATE INDEX db9_cop_prefix_smoke_status_created_idx ON db9_cop_prefix_smoke(status, created_at);
INSERT INTO db9_cop_prefix_smoke VALUES
    (1, 'active', 100, 'a'),
    (2, 'active', 200, 'b'),
    (3, 'paused', 150, 'c'),
    (4, 'active', 300, 'd');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' LIMIT 2;
SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' LIMIT 2;
CREATE TEMP TABLE db9_cop_prefix_on AS
SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' LIMIT 2;

EXPLAIN SELECT id, created_at
FROM db9_cop_prefix_smoke
WHERE status = 'active' AND created_at >= 200
LIMIT 2;
SELECT id, created_at
FROM db9_cop_prefix_smoke
WHERE status = 'active' AND created_at >= 200
LIMIT 2;
CREATE TEMP TABLE db9_cop_prefix_range_on AS
SELECT id, created_at
FROM db9_cop_prefix_smoke
WHERE status = 'active' AND created_at >= 200
LIMIT 2;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_prefix_off AS
SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' LIMIT 2;
CREATE TEMP TABLE db9_cop_prefix_range_off AS
SELECT id, created_at
FROM db9_cop_prefix_smoke
WHERE status = 'active' AND created_at >= 200
LIMIT 2;

SELECT 'prefix_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_prefix_on
            EXCEPT ALL
            SELECT * FROM db9_cop_prefix_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_prefix_off
            EXCEPT ALL
            SELECT * FROM db9_cop_prefix_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'prefix_range_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_prefix_range_on
            EXCEPT ALL
            SELECT * FROM db9_cop_prefix_range_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_prefix_range_off
            EXCEPT ALL
            SELECT * FROM db9_cop_prefix_range_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_prefix_smoke;
DROP TABLE db9_cop_prefix_on;
DROP TABLE db9_cop_prefix_off;
DROP TABLE db9_cop_prefix_range_on;
DROP TABLE db9_cop_prefix_range_off;

-- DB9 cop pushdown: preserve a global Limit above the local cop limit.

DROP TABLE IF EXISTS db9_cop_limit_smoke;
DROP TABLE IF EXISTS db9_cop_limit_on;
DROP TABLE IF EXISTS db9_cop_limit_off;
CREATE TABLE db9_cop_limit_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_limit_smoke_n_idx ON db9_cop_limit_smoke(n);
INSERT INTO db9_cop_limit_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30), (4, 'd', 40);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, v FROM db9_cop_limit_smoke WHERE n >= 20 LIMIT 1;
SELECT id, v FROM db9_cop_limit_smoke WHERE n >= 20 LIMIT 1;
CREATE TEMP TABLE db9_cop_limit_on AS
SELECT id, v FROM db9_cop_limit_smoke WHERE n >= 20 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_limit_off AS
SELECT id, v FROM db9_cop_limit_smoke WHERE n >= 20 LIMIT 1;

SELECT 'limit_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_limit_on
            EXCEPT ALL
            SELECT * FROM db9_cop_limit_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_limit_off
            EXCEPT ALL
            SELECT * FROM db9_cop_limit_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_limit_smoke;
DROP TABLE db9_cop_limit_on;
DROP TABLE db9_cop_limit_off;

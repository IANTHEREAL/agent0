-- DB9 cop pushdown: projection pruning keeps only requested output columns.

DROP TABLE IF EXISTS db9_cop_proj_smoke;
DROP TABLE IF EXISTS db9_cop_proj_on;
DROP TABLE IF EXISTS db9_cop_proj_off;
CREATE TABLE db9_cop_proj_smoke(id INT PRIMARY KEY, v TEXT, n INT, pad TEXT);
CREATE INDEX db9_cop_proj_smoke_n_idx ON db9_cop_proj_smoke(n);
INSERT INTO db9_cop_proj_smoke VALUES (1, 'a', 10, 'aa'), (2, 'b', 20, 'bb'), (3, 'c', 30, 'cc');

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT v FROM db9_cop_proj_smoke WHERE n = 20 LIMIT 1;
SELECT v FROM db9_cop_proj_smoke WHERE n = 20 LIMIT 1;
CREATE TEMP TABLE db9_cop_proj_on AS
SELECT v FROM db9_cop_proj_smoke WHERE n = 20 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_proj_off AS
SELECT v FROM db9_cop_proj_smoke WHERE n = 20 LIMIT 1;

SELECT 'projection_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_proj_on
            EXCEPT ALL
            SELECT * FROM db9_cop_proj_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_proj_off
            EXCEPT ALL
            SELECT * FROM db9_cop_proj_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_proj_smoke;
DROP TABLE db9_cop_proj_on;
DROP TABLE db9_cop_proj_off;

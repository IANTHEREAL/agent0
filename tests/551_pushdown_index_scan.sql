-- DB9 cop pushdown: exact secondary-index lookup with local fallback parity.

DROP TABLE IF EXISTS db9_cop_idx_smoke;
DROP TABLE IF EXISTS db9_cop_idx_on;
DROP TABLE IF EXISTS db9_cop_idx_off;
CREATE TABLE db9_cop_idx_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_idx_smoke_n_idx ON db9_cop_idx_smoke(n);
INSERT INTO db9_cop_idx_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, v FROM db9_cop_idx_smoke WHERE n = 20 LIMIT 1;
SELECT id, v FROM db9_cop_idx_smoke WHERE n = 20 LIMIT 1;
CREATE TEMP TABLE db9_cop_idx_on AS
SELECT id, v FROM db9_cop_idx_smoke WHERE n = 20 LIMIT 1;

SET db9.enable_cop_pushdown = off;
EXPLAIN SELECT id, v FROM db9_cop_idx_smoke WHERE n = 20 LIMIT 1;
CREATE TEMP TABLE db9_cop_idx_off AS
SELECT id, v FROM db9_cop_idx_smoke WHERE n = 20 LIMIT 1;

SELECT 'index_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_idx_on
            EXCEPT ALL
            SELECT * FROM db9_cop_idx_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_idx_off
            EXCEPT ALL
            SELECT * FROM db9_cop_idx_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_idx_smoke;
DROP TABLE db9_cop_idx_on;
DROP TABLE db9_cop_idx_off;

-- DB9 cop pushdown: IN-list secondary-index lookup exposes in-list access detail.

DROP TABLE IF EXISTS db9_cop_in_list_smoke;
DROP TABLE IF EXISTS db9_cop_in_list_on;
DROP TABLE IF EXISTS db9_cop_in_list_off;
CREATE TABLE db9_cop_in_list_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_in_list_smoke_n_idx ON db9_cop_in_list_smoke(n);
INSERT INTO db9_cop_in_list_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30), (4, 'd', 40);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, v FROM db9_cop_in_list_smoke WHERE n IN (20, 30) LIMIT 2;
SELECT id, v FROM db9_cop_in_list_smoke WHERE n IN (20, 30) LIMIT 2;
CREATE TEMP TABLE db9_cop_in_list_on AS
SELECT id, v FROM db9_cop_in_list_smoke WHERE n IN (20, 30) LIMIT 2;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_in_list_off AS
SELECT id, v FROM db9_cop_in_list_smoke WHERE n IN (20, 30) LIMIT 2;

SELECT 'in_list_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_in_list_on
            EXCEPT ALL
            SELECT * FROM db9_cop_in_list_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_in_list_off
            EXCEPT ALL
            SELECT * FROM db9_cop_in_list_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_in_list_smoke;
DROP TABLE db9_cop_in_list_on;
DROP TABLE db9_cop_in_list_off;

-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: supported function whitelist stays correct under pushdown.

DROP TABLE IF EXISTS db9_cop_func_smoke;
DROP TABLE IF EXISTS db9_cop_func_on;
DROP TABLE IF EXISTS db9_cop_func_off;
CREATE TABLE db9_cop_func_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_func_smoke_n_idx ON db9_cop_func_smoke(n);
INSERT INTO db9_cop_func_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT length(v), abs(n), coalesce(NULL, v), nullif(v, 'z')
FROM db9_cop_func_smoke
WHERE n = 20
LIMIT 1;
SELECT lower(v), upper(v), length(v), abs(n), coalesce(NULL, v), nullif(v, 'z')
FROM db9_cop_func_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_func_on AS
SELECT lower(v), upper(v), length(v), abs(n), coalesce(NULL, v), nullif(v, 'z')
FROM db9_cop_func_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_func_off AS
SELECT lower(v), upper(v), length(v), abs(n), coalesce(NULL, v), nullif(v, 'z')
FROM db9_cop_func_smoke
WHERE n = 20
LIMIT 1;

SELECT 'function_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_func_on
            EXCEPT ALL
            SELECT * FROM db9_cop_func_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_func_off
            EXCEPT ALL
            SELECT * FROM db9_cop_func_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_func_smoke;
DROP TABLE db9_cop_func_on;
DROP TABLE db9_cop_func_off;

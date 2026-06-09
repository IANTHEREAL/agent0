-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: unsupported expressions keep correct local semantics.

DROP TABLE IF EXISTS db9_cop_fallback_smoke;
DROP TABLE IF EXISTS db9_cop_fallback_on;
DROP TABLE IF EXISTS db9_cop_fallback_off;
CREATE TABLE db9_cop_fallback_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_fallback_smoke_n_idx ON db9_cop_fallback_smoke(n);
INSERT INTO db9_cop_fallback_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT substring(v, 'b') FROM db9_cop_fallback_smoke WHERE n = 20 LIMIT 1;
SELECT substring(v, 'b') FROM db9_cop_fallback_smoke WHERE n = 20 LIMIT 1;
CREATE TEMP TABLE db9_cop_fallback_on AS
SELECT substring(v, 'b') FROM db9_cop_fallback_smoke WHERE n = 20 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_fallback_off AS
SELECT substring(v, 'b') FROM db9_cop_fallback_smoke WHERE n = 20 LIMIT 1;

SELECT 'fallback_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_fallback_on
            EXCEPT ALL
            SELECT * FROM db9_cop_fallback_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_fallback_off
            EXCEPT ALL
            SELECT * FROM db9_cop_fallback_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_fallback_smoke;
DROP TABLE db9_cop_fallback_on;
DROP TABLE db9_cop_fallback_off;

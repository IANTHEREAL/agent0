-- DB9 cop pushdown: seq scan + filter + output + limit.

DROP TABLE IF EXISTS db9_cop_seq_smoke;
DROP TABLE IF EXISTS db9_cop_seq_on;
DROP TABLE IF EXISTS db9_cop_seq_off;
CREATE TABLE db9_cop_seq_smoke(id INT PRIMARY KEY, v TEXT, n INT);
INSERT INTO db9_cop_seq_smoke VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT id, v FROM db9_cop_seq_smoke WHERE v >= 'b' LIMIT 1;
SELECT id, v FROM db9_cop_seq_smoke WHERE v >= 'b' LIMIT 1;
CREATE TEMP TABLE db9_cop_seq_on AS
SELECT id, v FROM db9_cop_seq_smoke WHERE v >= 'b' LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_seq_off AS
SELECT id, v FROM db9_cop_seq_smoke WHERE v >= 'b' LIMIT 1;

SELECT 'seq_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_seq_on
            EXCEPT ALL
            SELECT * FROM db9_cop_seq_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_seq_off
            EXCEPT ALL
            SELECT * FROM db9_cop_seq_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_seq_smoke;
DROP TABLE db9_cop_seq_on;
DROP TABLE db9_cop_seq_off;

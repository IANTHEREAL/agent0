-- DB9 cop pushdown: COPY FROM STDIN inside an explicit transaction keeps
-- transaction-local visibility aligned with pushdown off.

DROP TABLE IF EXISTS db9_cop_copy_txn;
DROP TABLE IF EXISTS db9_cop_copy_txn_on;
DROP TABLE IF EXISTS db9_cop_copy_txn_off;

CREATE TABLE db9_cop_copy_txn(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_copy_txn_n_idx ON db9_cop_copy_txn(n);

BEGIN;
COPY db9_cop_copy_txn (id, v, n) FROM STDIN;
1	a	10
2	b	20
3	c	30
4	d	40
\.

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_copy_txn_on AS
SELECT id, v FROM db9_cop_copy_txn WHERE n = 40 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_copy_txn_off AS
SELECT id, v FROM db9_cop_copy_txn WHERE n = 40 LIMIT 1;
COMMIT;

SELECT 'copy_txn_visibility_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_copy_txn_on
            EXCEPT ALL
            SELECT * FROM db9_cop_copy_txn_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_copy_txn_off
            EXCEPT ALL
            SELECT * FROM db9_cop_copy_txn_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_copy_txn;
DROP TABLE db9_cop_copy_txn_on;
DROP TABLE db9_cop_copy_txn_off;

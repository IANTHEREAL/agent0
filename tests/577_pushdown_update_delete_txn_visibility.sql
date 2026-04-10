-- DB9 cop pushdown: UPDATE-only and DELETE-only transactions must keep
-- same-transaction reads aligned with pushdown-off visibility.

DROP TABLE IF EXISTS db9_cop_update_only_txn;
DROP TABLE IF EXISTS db9_cop_update_only_txn_on;
DROP TABLE IF EXISTS db9_cop_update_only_txn_off;
DROP TABLE IF EXISTS db9_cop_delete_only_txn;
DROP TABLE IF EXISTS db9_cop_delete_only_txn_on;
DROP TABLE IF EXISTS db9_cop_delete_only_txn_off;

CREATE TABLE db9_cop_update_only_txn(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_update_only_txn_n_idx ON db9_cop_update_only_txn(n);
INSERT INTO db9_cop_update_only_txn VALUES (1, 'a', 10), (2, 'b', 20);

BEGIN;
UPDATE db9_cop_update_only_txn SET v = 'updated', n = 40 WHERE id = 1;

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_update_only_txn_on AS
SELECT id, v FROM db9_cop_update_only_txn WHERE n >= 40 ORDER BY id;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_update_only_txn_off AS
SELECT id, v FROM db9_cop_update_only_txn WHERE n >= 40 ORDER BY id;
COMMIT;

SELECT 'update_only_txn_visibility_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_update_only_txn_on
            EXCEPT ALL
            SELECT * FROM db9_cop_update_only_txn_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_update_only_txn_off
            EXCEPT ALL
            SELECT * FROM db9_cop_update_only_txn_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

CREATE TABLE db9_cop_delete_only_txn(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_delete_only_txn_n_idx ON db9_cop_delete_only_txn(n);
INSERT INTO db9_cop_delete_only_txn VALUES (1, 'a', 40), (2, 'b', 50);

BEGIN;
DELETE FROM db9_cop_delete_only_txn WHERE id = 1;

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_delete_only_txn_on AS
SELECT id, v FROM db9_cop_delete_only_txn WHERE n >= 40 ORDER BY id;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_delete_only_txn_off AS
SELECT id, v FROM db9_cop_delete_only_txn WHERE n >= 40 ORDER BY id;
COMMIT;

SELECT 'delete_only_txn_visibility_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_delete_only_txn_on
            EXCEPT ALL
            SELECT * FROM db9_cop_delete_only_txn_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_delete_only_txn_off
            EXCEPT ALL
            SELECT * FROM db9_cop_delete_only_txn_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_update_only_txn;
DROP TABLE db9_cop_update_only_txn_on;
DROP TABLE db9_cop_update_only_txn_off;
DROP TABLE db9_cop_delete_only_txn;
DROP TABLE db9_cop_delete_only_txn_on;
DROP TABLE db9_cop_delete_only_txn_off;

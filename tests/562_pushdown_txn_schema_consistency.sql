-- DB9 cop pushdown: explicit-transaction visibility stays consistent after
-- row writes and metadata-only schema changes.

DROP TABLE IF EXISTS db9_cop_txn_schema;
DROP TABLE IF EXISTS db9_cop_txn_schema_tx_on;
DROP TABLE IF EXISTS db9_cop_txn_schema_tx_off;
DROP TABLE IF EXISTS db9_cop_txn_schema_ddl_on;
DROP TABLE IF EXISTS db9_cop_txn_schema_ddl_off;

CREATE TABLE db9_cop_txn_schema(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_txn_schema_n_idx ON db9_cop_txn_schema(n);
INSERT INTO db9_cop_txn_schema VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

BEGIN;
INSERT INTO db9_cop_txn_schema VALUES (4, 'd', 40);

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_txn_schema_tx_on AS
SELECT id, v FROM db9_cop_txn_schema WHERE n = 40 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_txn_schema_tx_off AS
SELECT id, v FROM db9_cop_txn_schema WHERE n = 40 LIMIT 1;
COMMIT;

SELECT 'txn_visibility_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_txn_schema_tx_on
            EXCEPT ALL
            SELECT * FROM db9_cop_txn_schema_tx_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_txn_schema_tx_off
            EXCEPT ALL
            SELECT * FROM db9_cop_txn_schema_tx_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

BEGIN;
ALTER TABLE db9_cop_txn_schema ADD COLUMN pad TEXT DEFAULT 'x';

SET db9.enable_cop_pushdown = on;
CREATE TEMP TABLE db9_cop_txn_schema_ddl_on AS
SELECT id, pad FROM db9_cop_txn_schema WHERE n = 30 LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_txn_schema_ddl_off AS
SELECT id, pad FROM db9_cop_txn_schema WHERE n = 30 LIMIT 1;
COMMIT;

SELECT 'metadata_ddl_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_txn_schema_ddl_on
            EXCEPT ALL
            SELECT * FROM db9_cop_txn_schema_ddl_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_txn_schema_ddl_off
            EXCEPT ALL
            SELECT * FROM db9_cop_txn_schema_ddl_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_txn_schema;
DROP TABLE db9_cop_txn_schema_tx_on;
DROP TABLE db9_cop_txn_schema_tx_off;
DROP TABLE db9_cop_txn_schema_ddl_on;
DROP TABLE db9_cop_txn_schema_ddl_off;

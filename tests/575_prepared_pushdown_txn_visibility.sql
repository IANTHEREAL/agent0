-- Prepared DB9 cop pushdown: cached prepared SELECT must stop using prepared
-- plan cache after the current transaction dirties the same table.

SET db9.prepared_plan_cache_min_exec = 5;

-- 1) UPDATE / UPSERT / DELETE on the same table.
DROP TABLE IF EXISTS db9_prepared_txn_dml;

CREATE TABLE db9_prepared_txn_dml(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_prepared_txn_dml_n_idx ON db9_prepared_txn_dml(n);
INSERT INTO db9_prepared_txn_dml VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

PREPARE db9_prepared_txn_dml_on AS
SELECT id, v FROM db9_prepared_txn_dml WHERE n >= 40 ORDER BY id;

PREPARE db9_prepared_txn_dml_off AS
SELECT id, v FROM db9_prepared_txn_dml WHERE TRUE AND n >= 40 ORDER BY id;

SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_dml_on;
EXECUTE db9_prepared_txn_dml_on;
EXECUTE db9_prepared_txn_dml_on;
EXECUTE db9_prepared_txn_dml_on;
EXECUTE db9_prepared_txn_dml_on;

BEGIN;
UPDATE db9_prepared_txn_dml SET v = 'u1x', n = 40 WHERE id = 1;
INSERT INTO db9_prepared_txn_dml(id, v, n)
VALUES (1, 'u1_upsert', 40)
ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v, n = EXCLUDED.n;
DELETE FROM db9_prepared_txn_dml WHERE id = 2;

SELECT 'prepared_dml_on' AS phase;
SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_dml_on;

SELECT 'prepared_dml_off' AS phase;
SET db9.enable_cop_pushdown = off;
EXECUTE db9_prepared_txn_dml_off;
COMMIT;

DEALLOCATE db9_prepared_txn_dml_on;
DEALLOCATE db9_prepared_txn_dml_off;
DROP TABLE db9_prepared_txn_dml;

-- 2) Metadata-only ALTER TABLE on the same table.
DROP TABLE IF EXISTS db9_prepared_txn_ddl;

CREATE TABLE db9_prepared_txn_ddl(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_prepared_txn_ddl_n_idx ON db9_prepared_txn_ddl(n);
INSERT INTO db9_prepared_txn_ddl VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);

PREPARE db9_prepared_txn_ddl_on AS
SELECT id, v FROM db9_prepared_txn_ddl WHERE n = 30 ORDER BY id;

PREPARE db9_prepared_txn_ddl_off AS
SELECT id, v FROM db9_prepared_txn_ddl WHERE TRUE AND n = 30 ORDER BY id;

SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_ddl_on;
EXECUTE db9_prepared_txn_ddl_on;
EXECUTE db9_prepared_txn_ddl_on;
EXECUTE db9_prepared_txn_ddl_on;
EXECUTE db9_prepared_txn_ddl_on;

BEGIN;
ALTER TABLE db9_prepared_txn_ddl ADD COLUMN pad TEXT DEFAULT 'x';

SELECT 'prepared_ddl_on' AS phase;
SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_ddl_on;

SELECT 'prepared_ddl_off' AS phase;
SET db9.enable_cop_pushdown = off;
EXECUTE db9_prepared_txn_ddl_off;
COMMIT;

DEALLOCATE db9_prepared_txn_ddl_on;
DEALLOCATE db9_prepared_txn_ddl_off;
DROP TABLE db9_prepared_txn_ddl;

-- 3) COPY inside an explicit transaction on the same table.
DROP TABLE IF EXISTS db9_prepared_txn_copy;

CREATE TABLE db9_prepared_txn_copy(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_prepared_txn_copy_n_idx ON db9_prepared_txn_copy(n);

PREPARE db9_prepared_txn_copy_on AS
SELECT id, v FROM db9_prepared_txn_copy WHERE n >= 40 ORDER BY id;

PREPARE db9_prepared_txn_copy_off AS
SELECT id, v FROM db9_prepared_txn_copy WHERE TRUE AND n >= 40 ORDER BY id;

SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_copy_on;
EXECUTE db9_prepared_txn_copy_on;
EXECUTE db9_prepared_txn_copy_on;
EXECUTE db9_prepared_txn_copy_on;
EXECUTE db9_prepared_txn_copy_on;

BEGIN;
COPY db9_prepared_txn_copy (id, v, n) FROM STDIN;
1	a	10
2	b	20
3	c	30
4	d	40
\.

SELECT 'prepared_copy_on' AS phase;
SET db9.enable_cop_pushdown = on;
EXECUTE db9_prepared_txn_copy_on;

SELECT 'prepared_copy_off' AS phase;
SET db9.enable_cop_pushdown = off;
EXECUTE db9_prepared_txn_copy_off;
COMMIT;

DEALLOCATE db9_prepared_txn_copy_on;
DEALLOCATE db9_prepared_txn_copy_off;
DROP TABLE db9_prepared_txn_copy;

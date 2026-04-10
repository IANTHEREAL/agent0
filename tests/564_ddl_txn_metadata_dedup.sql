-- Regression: within one explicit transaction, repeated schema updates on the
-- same table must not duplicate metadata scans or make DROP INDEX ambiguous.

DROP TABLE IF EXISTS t564_ddltxn;

BEGIN;

CREATE TABLE t564_ddltxn (
    id INT PRIMARY KEY,
    name TEXT
);

ALTER TABLE t564_ddltxn ADD COLUMN nick TEXT;
CREATE INDEX idx_t564_name ON t564_ddltxn(name);

SELECT count(*)
FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name = 't564_ddltxn';

SELECT count(*)
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 't564_ddltxn'
  AND indexname = 't564_ddltxn_pkey';

SELECT count(*)
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 't564_ddltxn'
  AND indexname = 'idx_t564_name';

SELECT count(*)
FROM information_schema.table_constraints
WHERE table_schema = 'public'
  AND table_name = 't564_ddltxn'
  AND constraint_name = 't564_ddltxn_pkey'
  AND constraint_type = 'PRIMARY KEY';

DROP INDEX public.idx_t564_name;

SELECT count(*)
FROM pg_indexes
WHERE schemaname = 'public'
  AND tablename = 't564_ddltxn'
  AND indexname = 'idx_t564_name';

COMMIT;

DROP TABLE t564_ddltxn;

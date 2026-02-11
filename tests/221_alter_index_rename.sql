-- ALTER INDEX RENAME tests (issue #587)
-- Verify: basic rename, PK rename, cross-table name conflict, catalog visibility

DROP TABLE IF EXISTS t221_a CASCADE;
DROP TABLE IF EXISTS t221_b CASCADE;

CREATE TABLE t221_a (id INT PRIMARY KEY, val INT);
CREATE INDEX idx_221_a_val ON t221_a (val);

CREATE TABLE t221_b (id INT PRIMARY KEY, val INT);
CREATE INDEX idx_221_b_val ON t221_b (val);

-- 1. Basic rename: index visible under new name in pg_indexes
ALTER INDEX idx_221_a_val RENAME TO idx_221_a_val_renamed;
SELECT indexname FROM pg_indexes WHERE tablename = 't221_a' AND indexname = 'idx_221_a_val_renamed';

-- 2. Old name gone from catalog
SELECT count(*) AS old_gone FROM pg_indexes WHERE indexname = 'idx_221_a_val';

-- 3. DROP old name should fail
DROP INDEX idx_221_a_val;

-- 4. DROP new name should succeed
DROP INDEX idx_221_a_val_renamed;

-- 5. Cross-table name conflict: rename into a name that exists on another table
CREATE INDEX idx_221_a_val2 ON t221_a (val);
ALTER INDEX idx_221_a_val2 RENAME TO idx_221_b_val;

-- 6. PK rename: rename primary key constraint index
ALTER INDEX t221_a_pkey RENAME TO t221_a_pk_renamed;
SELECT indexname FROM pg_indexes WHERE tablename = 't221_a' AND indexname = 't221_a_pk_renamed';

-- 7. Old PK name gone
SELECT count(*) AS old_pk_gone FROM pg_indexes WHERE tablename = 't221_a' AND indexname = 't221_a_pkey';

-- 8. Nonexistent index should error
ALTER INDEX idx_221_nonexistent RENAME TO idx_221_whatever;

-- Cleanup
DROP TABLE t221_a CASCADE;
DROP TABLE t221_b CASCADE;

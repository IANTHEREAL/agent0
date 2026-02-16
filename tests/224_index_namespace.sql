-- Index namespace uniqueness tests (issue #775)
-- Verify: cross-table duplicate rejected, IF NOT EXISTS, PK/table/view/sequence conflicts

DROP TABLE IF EXISTS t224_a CASCADE;
DROP TABLE IF EXISTS t224_b CASCADE;
DROP VIEW IF EXISTS t224_v CASCADE;
DROP SEQUENCE IF EXISTS t224_seq;

CREATE TABLE t224_a (id INT PRIMARY KEY, val INT);
CREATE TABLE t224_b (id INT PRIMARY KEY, val INT);

-- 1. Create index on table A
CREATE INDEX idx_224_shared ON t224_a (val);

-- 2. Cross-table duplicate: same index name on table B should fail (42P07)
CREATE INDEX idx_224_shared ON t224_b (val);

-- 3. IF NOT EXISTS: same name should succeed silently
CREATE INDEX IF NOT EXISTS idx_224_shared ON t224_b (val);

-- 4. Index vs PK constraint name conflict
CREATE INDEX t224_a_pkey ON t224_b (val);

-- 5. Index vs table name conflict
CREATE INDEX t224_a ON t224_b (val);

-- 6. Index vs view name conflict
CREATE VIEW t224_v AS SELECT 1 AS x;
CREATE INDEX t224_v ON t224_b (val);

-- 7. Index vs sequence name conflict
CREATE SEQUENCE t224_seq;
CREATE INDEX t224_seq ON t224_b (val);

-- 8. After DROP INDEX, name is available for reuse
DROP INDEX idx_224_shared;
CREATE INDEX idx_224_shared ON t224_b (val);

-- 9. After DROP TABLE, index names freed
DROP TABLE t224_a CASCADE;
CREATE TABLE t224_a (id INT PRIMARY KEY, val INT);
CREATE INDEX t224_a_pkey_idx ON t224_a (val);

-- 10. CREATE TABLE PK name vs existing view
CREATE VIEW t224_pk_view AS SELECT 1 AS x;
CREATE TABLE t224_pk_view_test (id INT CONSTRAINT t224_pk_view PRIMARY KEY);

-- 11. CREATE TABLE AS reserves its PK name
CREATE TABLE t224_ctas AS SELECT 1 AS id;
CREATE INDEX t224_ctas_pkey ON t224_a (val);

-- 12. SELECT INTO reserves its PK name
SELECT 1 AS id INTO t224_selinto;
CREATE INDEX t224_selinto_pkey ON t224_a (val);

-- 13. ALTER TABLE RENAME CONSTRAINT (unique) maintains reservation key
CREATE UNIQUE INDEX t224_uc_old ON t224_a (val);
ALTER TABLE t224_a RENAME CONSTRAINT t224_uc_old TO t224_uc_new;
-- Old name freed — can create index with old name
CREATE INDEX t224_uc_old ON t224_a (id);
-- New name reserved — cannot reuse
CREATE INDEX t224_uc_new ON t224_b (val);

SELECT 'index namespace tests completed' AS result;

-- Cleanup
DROP TABLE t224_a CASCADE;
DROP TABLE t224_b CASCADE;
DROP TABLE IF EXISTS t224_ctas CASCADE;
DROP TABLE IF EXISTS t224_selinto CASCADE;
DROP TABLE IF EXISTS t224_pk_view_test CASCADE;
DROP VIEW IF EXISTS t224_v CASCADE;
DROP VIEW IF EXISTS t224_pk_view CASCADE;
DROP SEQUENCE IF EXISTS t224_seq;

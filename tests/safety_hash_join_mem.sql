-- Safety regression tests for hash join memory limit (PR #2043).
-- Default: 256 MB. Comparison: memory_bytes > max_memory (strictly >).

CREATE TABLE hjm_sa (id INT PRIMARY KEY, val TEXT);
CREATE TABLE hjm_sb (id INT PRIMARY KEY, val TEXT);
INSERT INTO hjm_sa SELECT g, repeat('x', 100) FROM generate_series(1, 50) g;
INSERT INTO hjm_sb SELECT g, repeat('y', 100) FROM generate_series(1, 50) g;

-- ================================================================
-- GUC round-trip: default, SET, SHOW, RESET
-- ================================================================
SHOW db9.hash_join_work_mem;
SET db9.hash_join_work_mem = '64MB';
SHOW db9.hash_join_work_mem;
SET db9.hash_join_work_mem = '0';
SHOW db9.hash_join_work_mem;
RESET db9.hash_join_work_mem;
SHOW db9.hash_join_work_mem;

-- ================================================================
-- Byte-size parsing edge cases
-- ================================================================
SET db9.hash_join_work_mem = '1GB';
SHOW db9.hash_join_work_mem;
SET db9.hash_join_work_mem = '512KB';
SHOW db9.hash_join_work_mem;
RESET db9.hash_join_work_mem;

-- ================================================================
-- Boundary: limit = 1KB, 50 rows * ~100 bytes >> 1KB → MUST fail
-- ================================================================
SET db9.hash_join_work_mem = '1KB';

-- Equi-join (HashJoinOperator)
SELECT count(*) FROM hjm_sa JOIN hjm_sb ON hjm_sa.id = hjm_sb.id;

-- Semi-join via EXISTS (HashSemiJoinOperator)
SELECT count(*) FROM hjm_sa WHERE EXISTS (SELECT 1 FROM hjm_sb WHERE hjm_sb.id = hjm_sa.id);

-- Anti-join via NOT EXISTS (HashSemiJoinOperator)
SELECT count(*) FROM hjm_sa WHERE NOT EXISTS (SELECT 1 FROM hjm_sb WHERE hjm_sb.id = hjm_sa.id);

-- ================================================================
-- Boundary: 0 = unlimited, same join MUST succeed
-- ================================================================
SET db9.hash_join_work_mem = '0';
SELECT count(*) FROM hjm_sa JOIN hjm_sb ON hjm_sa.id = hjm_sb.id;
SELECT count(*) FROM hjm_sa WHERE EXISTS (SELECT 1 FROM hjm_sb WHERE hjm_sb.id = hjm_sa.id);

-- ================================================================
-- Boundary: very large limit, should succeed
-- ================================================================
SET db9.hash_join_work_mem = '10GB';
SELECT count(*) FROM hjm_sa JOIN hjm_sb ON hjm_sa.id = hjm_sb.id;

RESET db9.hash_join_work_mem;
DROP TABLE hjm_sa;
DROP TABLE hjm_sb;

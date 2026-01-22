-- PostgreSQL compatible tests from mixed_version_distributed_merge
-- 7 tests

-- Test 1: statement (line 5)
CREATE TABLE dist_merge_idx (
    a INT NOT NULL PRIMARY KEY,
    b INT NOT NULL,
    c INT NOT NULL UNIQUE
);

-- Test 2: statement (line 8)
INSERT INTO dist_merge_idx VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3);

-- let $index_backfill_dist_merge_mode
-- SHOW CLUSTER SETTING bulkio.index_backfill.distributed_merge.mode;

-- Test 3: statement (line 14)
-- SET CLUSTER SETTING bulkio.index_backfill.distributed_merge.mode = enabled;

-- Test 4: statement (line 17)
CREATE INDEX dist_merge_idx_idx ON dist_merge_idx (b);

-- Test 5: statement (line 21)
ALTER TABLE dist_merge_idx DROP CONSTRAINT dist_merge_idx_pkey;
ALTER TABLE dist_merge_idx ADD PRIMARY KEY (b);

-- Test 6: statement (line 24)
-- SET CLUSTER SETTING bulkio.index_backfill.distributed_merge.mode = '$index_backfill_dist_merge_mode';

-- Test 7: statement (line 27)
DROP TABLE dist_merge_idx;

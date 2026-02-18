-- CREATE INDEX CONCURRENTLY regression coverage
-- Covers:
-- 1) non-unique CIC path
-- 2) unique CIC path
-- 3) reinserting a deleted unique value after CIC

DROP TABLE IF EXISTS cic_test;
CREATE TABLE cic_test (id INTEGER PRIMARY KEY, val TEXT);
INSERT INTO cic_test VALUES (1, 'a'), (2, 'b'), (3, 'c');

-- Regular CREATE INDEX should still work.
CREATE INDEX idx_cic_val ON cic_test(val);
SELECT val FROM cic_test WHERE val = 'b';

-- Non-unique CIC.
CREATE INDEX CONCURRENTLY idx_cic_val2 ON cic_test(id);

-- Unique CIC.
CREATE UNIQUE INDEX CONCURRENTLY idx_cic_val_uniq ON cic_test(val);
SELECT indexname
FROM pg_indexes
WHERE tablename = 'cic_test' AND indexname = 'idx_cic_val_uniq';

DROP TABLE cic_test;

DROP TABLE IF EXISTS cic_stale_test;
CREATE TABLE cic_stale_test (id SERIAL PRIMARY KEY, code TEXT NOT NULL);
INSERT INTO cic_stale_test(code) VALUES ('A'), ('B'), ('C');
DELETE FROM cic_stale_test WHERE code = 'B';

-- Build unique index after delete, then reinsert deleted value.
CREATE UNIQUE INDEX CONCURRENTLY idx_cic_stale_code ON cic_stale_test(code);
SELECT indexname
FROM pg_indexes
WHERE tablename = 'cic_stale_test' AND indexname = 'idx_cic_stale_code';

INSERT INTO cic_stale_test(code) VALUES ('B');
SELECT count(*) || ':b_count' AS b_count_marker
FROM cic_stale_test
WHERE code = 'B';

DROP TABLE cic_stale_test;

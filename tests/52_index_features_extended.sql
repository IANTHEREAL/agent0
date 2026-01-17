-- Extended Index Features Tests
-- Tests for partial indexes, expression indexes, GIN/GIST, and planner behavior

DROP TABLE IF EXISTS idx_ext CASCADE;
CREATE TABLE idx_ext (
    id INT PRIMARY KEY,
    status TEXT,
    name TEXT,
    tags TEXT,
    data TEXT,
    score INT
);

INSERT INTO idx_ext VALUES (1, 'active', 'Alice', 'tag1,tag2', 'some data', 100);
INSERT INTO idx_ext VALUES (2, 'inactive', 'Bob', 'tag2,tag3', 'other data', 200);
INSERT INTO idx_ext VALUES (3, 'active', 'Charlie', 'tag1', 'more data', 150);
INSERT INTO idx_ext VALUES (4, NULL, 'David', 'tag3,tag4', 'test data', 300);
INSERT INTO idx_ext VALUES (5, 'active', 'Eve', 'tag1,tag2,tag3', 'final data', 250);

-- Standard btree index
CREATE INDEX idx_ext_score ON idx_ext (score);
SELECT 'CREATE_BTREE:success';

-- Partial index with WHERE clause
CREATE INDEX idx_ext_active ON idx_ext (name) WHERE status = 'active';
SELECT 'CREATE_PARTIAL:success';

-- Another partial index with IS NOT NULL
CREATE INDEX idx_ext_status_notnull ON idx_ext (status) WHERE status IS NOT NULL;
SELECT 'CREATE_PARTIAL_NOTNULL:success';

-- Expression index
CREATE INDEX idx_ext_lower_name ON idx_ext ((lower(name)));
SELECT 'CREATE_EXPR:success';

-- Compound expression index
CREATE INDEX idx_ext_upper_status ON idx_ext ((upper(status)));
SELECT 'CREATE_EXPR2:success';

-- GIN index
CREATE INDEX idx_ext_tags_gin ON idx_ext USING gin (tags);
SELECT 'CREATE_GIN:success';

-- GIST index
CREATE INDEX idx_ext_data_gist ON idx_ext USING gist (data);
SELECT 'CREATE_GIST:success';

-- Hash index
CREATE INDEX idx_ext_name_hash ON idx_ext USING hash (name);
SELECT 'CREATE_HASH:success';

-- Verify queries return correct results
SELECT 'RESULT_PARTIAL:' || id || ':' || name
FROM idx_ext WHERE status = 'active' ORDER BY id;

SELECT 'RESULT_EXPR:' || id || ':' || name
FROM idx_ext WHERE lower(name) = 'alice';

SELECT 'RESULT_BTREE:' || id || ':' || score
FROM idx_ext WHERE score = 150;

-- Verify access methods in pg_am
SELECT 'AM:' || amname || ':' || oid
FROM pg_catalog.pg_am
WHERE amname IN ('btree', 'hash', 'gin', 'gist')
ORDER BY amname;

-- Test IF NOT EXISTS for indexes
CREATE INDEX IF NOT EXISTS idx_ext_score ON idx_ext (score);
SELECT 'IF_NOT_EXISTS:success';

-- Complex partial index predicates
CREATE INDEX idx_ext_complex ON idx_ext (name) WHERE status = 'active' AND score > 100;
SELECT 'CREATE_COMPLEX:success';

-- Drop and verify cleanup
DROP INDEX idx_ext_score;
DROP INDEX idx_ext_active;
DROP INDEX idx_ext_status_notnull;
DROP INDEX idx_ext_lower_name;
DROP INDEX idx_ext_upper_status;
DROP INDEX idx_ext_tags_gin;
DROP INDEX idx_ext_data_gist;
DROP INDEX idx_ext_name_hash;
DROP INDEX idx_ext_complex;
SELECT 'DROP_ALL:success';

DROP TABLE idx_ext;

SELECT 'TEST_COMPLETE';

-- IN (...) on composite index prefix columns
-- Regression test for issue #2230:
--   InListScanOperator always calls scan_index() which assumes all index
--   columns are provided. When only a prefix is provided, PK decoding is
--   wrong — returns 0 rows (INT PK) or crashes (UUID PK).

-- ============================================================
-- Test group A: 2-column composite index with INT PK
-- ============================================================

DROP TABLE IF EXISTS inlist_comp2 CASCADE;

CREATE TABLE inlist_comp2 (
    id SERIAL PRIMARY KEY,
    state TEXT NOT NULL,
    priority INT NOT NULL DEFAULT 0,
    payload TEXT
);

CREATE INDEX idx_comp2_state_pri ON inlist_comp2 (state, priority);

INSERT INTO inlist_comp2 (state, priority, payload) VALUES
    ('open',      1, 'task-1'),
    ('open',      2, 'task-2'),
    ('open',      3, 'task-3'),
    ('leased',    1, 'task-4'),
    ('leased',    2, 'task-5'),
    ('stalled',   1, 'task-6'),
    ('completed', 1, 'task-7'),
    ('completed', 2, 'task-8');

-- Q1: IN on first column only (prefix of 2-col composite index)
SELECT state, payload FROM inlist_comp2
WHERE state IN ('open', 'leased')
ORDER BY state, id;

-- Q2: IN on first column with additional filter on non-index column
SELECT state, payload FROM inlist_comp2
WHERE state IN ('open', 'leased') AND payload LIKE '%task-1%'
ORDER BY state, id;

-- Q3: IN with single value (still uses InListScan path)
SELECT state, payload FROM inlist_comp2
WHERE state IN ('stalled')
ORDER BY id;

-- Q4: IN on first column + equality on second column (covers full key)
SELECT state, payload FROM inlist_comp2
WHERE state IN ('open', 'leased') AND priority = 1
ORDER BY state, id;

DROP TABLE inlist_comp2;

-- ============================================================
-- Test group B: 3-column composite index with INT PK
-- ============================================================

DROP TABLE IF EXISTS inlist_comp3 CASCADE;

CREATE TABLE inlist_comp3 (
    id SERIAL PRIMARY KEY,
    state TEXT NOT NULL,
    priority INT NOT NULL DEFAULT 0,
    payload TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_comp3_all ON inlist_comp3 (state, priority, payload);

INSERT INTO inlist_comp3 (state, priority, payload) VALUES
    ('open',      1, 'a'),
    ('open',      2, 'b'),
    ('open',      3, 'c'),
    ('closed',    1, 'd'),
    ('closed',    2, 'e');

-- Q5: IN on first column only (1 of 3 index columns)
SELECT state, payload FROM inlist_comp3
WHERE state IN ('open', 'closed')
ORDER BY state, id;

-- Q6: EQ on first + IN on second (2 of 3 index columns)
SELECT state, payload FROM inlist_comp3
WHERE state = 'open' AND priority IN (1, 3)
ORDER BY id;

DROP TABLE inlist_comp3;

-- ============================================================
-- Test group C: UUID primary key with composite index
-- ============================================================

DROP TABLE IF EXISTS inlist_uuid CASCADE;

CREATE TABLE inlist_uuid (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    state TEXT NOT NULL,
    category TEXT NOT NULL,
    data TEXT
);

CREATE INDEX idx_uuid_state_cat ON inlist_uuid (state, category);

INSERT INTO inlist_uuid (state, category, data) VALUES
    ('active',   'A', 'row-1'),
    ('active',   'B', 'row-2'),
    ('inactive', 'A', 'row-3'),
    ('inactive', 'B', 'row-4'),
    ('pending',  'A', 'row-5');

-- Q7: IN on first column of composite index with UUID PK
SELECT state, data FROM inlist_uuid
WHERE state IN ('active', 'pending')
ORDER BY state, data;

-- Q8: count to verify completeness
SELECT COUNT(*) AS cnt FROM inlist_uuid
WHERE state IN ('active', 'pending');

DROP TABLE inlist_uuid;

-- ============================================================
-- Test group D: UNIQUE composite index + IN prefix
-- ============================================================

DROP TABLE IF EXISTS inlist_uniq CASCADE;

CREATE TABLE inlist_uniq (
    id SERIAL PRIMARY KEY,
    code TEXT NOT NULL,
    region TEXT NOT NULL,
    val INT
);

CREATE UNIQUE INDEX idx_uniq_code_region ON inlist_uniq (code, region);

INSERT INTO inlist_uniq (code, region, val) VALUES
    ('A', 'US', 10),
    ('A', 'EU', 20),
    ('B', 'US', 30),
    ('B', 'EU', 40),
    ('C', 'US', 50);

-- Q9: IN on first column of UNIQUE composite index (prefix scan)
SELECT code, region, val FROM inlist_uniq
WHERE code IN ('A', 'C')
ORDER BY code, region;

DROP TABLE inlist_uniq;

-- ============================================================
-- Test group E: IN covering ALL index columns (scan_index path)
-- ============================================================

DROP TABLE IF EXISTS inlist_full CASCADE;

CREATE TABLE inlist_full (
    id SERIAL PRIMARY KEY,
    a TEXT NOT NULL,
    b TEXT NOT NULL,
    val INT
);

CREATE INDEX idx_full_ab ON inlist_full (a, b);

INSERT INTO inlist_full (a, b, val) VALUES
    ('x', 'p', 1),
    ('x', 'q', 2),
    ('y', 'p', 3),
    ('y', 'q', 4);

-- Q10: IN covers all index columns via EQ prefix + IN on last column
SELECT a, b, val FROM inlist_full
WHERE a = 'x' AND b IN ('p', 'q')
ORDER BY b;

DROP TABLE inlist_full;

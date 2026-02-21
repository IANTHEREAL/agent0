-- Subquery Decorrelation Tests
-- Purpose: Verify EXISTS/NOT EXISTS decorrelation into SemiJoin/AntiJoin

DROP TABLE IF EXISTS dc_t3;
DROP TABLE IF EXISTS dc_t2;
DROP TABLE IF EXISTS dc_t1;

CREATE TABLE dc_t1 (
    id INT PRIMARY KEY,
    name TEXT,
    a INT,
    b INT
);

CREATE TABLE dc_t2 (
    id INT,
    ref_id INT,
    active BOOLEAN DEFAULT TRUE,
    a INT,
    b INT,
    cat TEXT,
    val INT
);

CREATE TABLE dc_t3 (
    ref INT,
    label TEXT
);

INSERT INTO dc_t1 VALUES (1, 'Alice', 10, 20);
INSERT INTO dc_t1 VALUES (2, 'Bob', 30, 40);
INSERT INTO dc_t1 VALUES (3, 'Charlie', 50, 60);
INSERT INTO dc_t1 VALUES (4, 'Diana', 70, 80);

INSERT INTO dc_t2 VALUES (1, 1, true, 10, 20, 'X', 100);
INSERT INTO dc_t2 VALUES (2, 1, false, 10, 20, 'Y', 200);
INSERT INTO dc_t2 VALUES (3, 2, true, 30, 40, 'X', 300);
INSERT INTO dc_t2 VALUES (4, 3, true, 50, 60, 'Z', 400);

INSERT INTO dc_t3 VALUES (1, 'ref1');
INSERT INTO dc_t3 VALUES (2, 'ref2');

-- ============================================================
-- MUST-DECORRELATE CASES (correct results + EXPLAIN shows Semi/Anti Join)
-- ============================================================

-- Test 1: Simple EXISTS
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- Test 2: NOT EXISTS
SELECT * FROM dc_t1 WHERE NOT EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- Test 3: EXISTS with inner-only predicate
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id AND dc_t2.active = true) ORDER BY dc_t1.id;

-- Test 4: Multiple EXISTS in same WHERE
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) AND EXISTS (SELECT 1 FROM dc_t3 WHERE dc_t3.ref = dc_t1.id) ORDER BY dc_t1.id;

-- Test 5: Multi-column equi-correlation
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.a = dc_t1.a AND dc_t2.b = dc_t1.b) ORDER BY dc_t1.id;

-- Test 6: Duplicate matches: outer row emitted exactly ONCE for semi
INSERT INTO dc_t2 VALUES (10, 1, true, 10, 20, 'DUP1', 500);
INSERT INTO dc_t2 VALUES (11, 1, true, 10, 20, 'DUP2', 600);
INSERT INTO dc_t2 VALUES (12, 1, true, 10, 20, 'DUP3', 700);
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- Test 7: Duplicate matches: anti suppresses outer row correctly
SELECT * FROM dc_t1 WHERE NOT EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- Test 8: NULL join keys: anti-join must NOT match (NULL != NULL)
INSERT INTO dc_t1 VALUES (5, 'NullRef', NULL, NULL);
SELECT * FROM dc_t1 WHERE NOT EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- ============================================================
-- EXPLAIN TESTS
-- ============================================================

-- Test 9: EXPLAIN shows Hash Semi Join
EXPLAIN SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id);

-- Test 10: EXPLAIN shows Hash Anti Join
EXPLAIN SELECT * FROM dc_t1 WHERE NOT EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id);

-- ============================================================
-- MUST-NOT-DECORRELATE CASES (correct result via per-row execution)
-- ============================================================

-- Test 11: Subquery with LIMIT (reject decorrelation)
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id LIMIT 1) ORDER BY dc_t1.id;

-- Test 12: Subquery with GROUP BY (reject decorrelation)
SELECT * FROM dc_t1 WHERE EXISTS (SELECT dc_t2.cat FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id GROUP BY dc_t2.cat) ORDER BY dc_t1.id;

-- Test 13: Non-equi correlation (reject decorrelation)
SELECT * FROM dc_t1 WHERE EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id > dc_t1.id) ORDER BY dc_t1.id;

-- Test 14: EXISTS combined with non-correlated filter
SELECT * FROM dc_t1 WHERE dc_t1.name != 'Diana' AND EXISTS (SELECT 1 FROM dc_t2 WHERE dc_t2.ref_id = dc_t1.id) ORDER BY dc_t1.id;

-- Cleanup
DROP TABLE IF EXISTS dc_t3;
DROP TABLE IF EXISTS dc_t2;
DROP TABLE IF EXISTS dc_t1;

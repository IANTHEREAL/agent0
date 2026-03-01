-- UPDATE PK/Unique-Key Fast Path Regression Tests
-- Tests the try_pk_fast_fetch optimization that uses point-get instead
-- of full table scan when WHERE targets a PK or unique key.
-- All results must be identical to the full-scan fallback path.
-- Ref: issue #1284 Step 2.

-- ============================================================
-- Setup
-- ============================================================
DROP TABLE IF EXISTS fp_test CASCADE;
CREATE TABLE fp_test (
    id INT PRIMARY KEY,
    val TEXT
);
INSERT INTO fp_test VALUES (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four'), (5, 'five');

-- ============================================================
-- Test 1: pk = const (single-column PK equality)
-- Fast path: batch_get_rows with 1 PK vector.
-- ============================================================
UPDATE fp_test SET val = 'ONE' WHERE id = 1;
SELECT 'pk_eq_single' AS test_name,
       (val = 'ONE') AS ok
FROM fp_test WHERE id = 1;

-- ============================================================
-- Test 2: composite PK = const
-- Fast path: batch_get_rows with 1 composite PK vector.
-- ============================================================
DROP TABLE IF EXISTS fp_composite CASCADE;
CREATE TABLE fp_composite (
    a INT,
    b INT,
    c TEXT,
    PRIMARY KEY (a, b)
);
INSERT INTO fp_composite VALUES (1, 10, 'orig'), (1, 20, 'orig'), (2, 10, 'orig');

UPDATE fp_composite SET c = 'hit' WHERE a = 1 AND b = 10;
SELECT 'pk_eq_composite' AS test_name,
       (SELECT c FROM fp_composite WHERE a = 1 AND b = 10) = 'hit'
       AND (SELECT c FROM fp_composite WHERE a = 1 AND b = 20) = 'orig'
       AS ok;

-- ============================================================
-- Test 3: pk IN (c1, c2, c3) — basic in-list
-- Fast path: batch_get_rows with multiple PK vectors.
-- ============================================================
UPDATE fp_test SET val = 'INLIST' WHERE id IN (2, 4);
SELECT 'pk_inlist_basic' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'INLIST') = 2 AS ok;

-- ============================================================
-- Test 4: pk IN (...) with duplicates — must deduplicate
-- Each row should be updated exactly once, not twice.
-- ============================================================
UPDATE fp_test SET val = val || '_dup' WHERE id IN (2, 2, 4, 4, 4);
SELECT 'pk_inlist_dedup' AS test_name,
       (SELECT val FROM fp_test WHERE id = 2) = 'INLIST_dup'
       AND (SELECT val FROM fp_test WHERE id = 4) = 'INLIST_dup'
       AS ok;

-- ============================================================
-- Test 5: pk IN (...) with NULLs — NULLs filtered out
-- NULL in IN-list never matches any row (SQL standard).
-- ============================================================
UPDATE fp_test SET val = 'null_test' WHERE id IN (3, NULL);
SELECT 'pk_inlist_null' AS test_name,
       (SELECT val FROM fp_test WHERE id = 3) = 'null_test'
       AND (SELECT val FROM fp_test WHERE id = 5) = 'five'
       AS ok;

-- ============================================================
-- Test 6: pk IN (NULL, NULL) — all NULLs, updates 0 rows
-- ============================================================
UPDATE fp_test SET val = 'should_not_happen' WHERE id IN (NULL, NULL);
SELECT 'pk_inlist_all_null' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'should_not_happen') = 0 AS ok;

-- ============================================================
-- Test 7: Unique index equality conjunction
-- Fast path via scan_index → batch_get_rows.
-- ============================================================
DROP TABLE IF EXISTS fp_unique CASCADE;
CREATE TABLE fp_unique (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL,
    name TEXT
);
CREATE UNIQUE INDEX idx_fp_email ON fp_unique (email);
INSERT INTO fp_unique (email, name) VALUES ('a@b.com', 'alice'), ('c@d.com', 'carol');

UPDATE fp_unique SET name = 'ALICE' WHERE email = 'a@b.com';
SELECT 'unique_idx_eq' AS test_name,
       (SELECT name FROM fp_unique WHERE email = 'a@b.com') = 'ALICE' AS ok;

-- ============================================================
-- Test 8: Multi-column unique index
-- ============================================================
DROP TABLE IF EXISTS fp_multi_unique CASCADE;
CREATE TABLE fp_multi_unique (
    id SERIAL PRIMARY KEY,
    tenant_id INT NOT NULL,
    slug TEXT NOT NULL,
    data TEXT
);
CREATE UNIQUE INDEX idx_fp_tenant_slug ON fp_multi_unique (tenant_id, slug);
INSERT INTO fp_multi_unique (tenant_id, slug, data) VALUES (1, 'a', 'orig'), (1, 'b', 'orig'), (2, 'a', 'orig');

UPDATE fp_multi_unique SET data = 'hit' WHERE tenant_id = 1 AND slug = 'a';
SELECT 'unique_idx_multi' AS test_name,
       (SELECT data FROM fp_multi_unique WHERE tenant_id = 1 AND slug = 'a') = 'hit'
       AND (SELECT data FROM fp_multi_unique WHERE tenant_id = 1 AND slug = 'b') = 'orig'
       AS ok;

-- ============================================================
-- Test 9: Disqualifier — FROM clause (must fallback)
-- ============================================================
DROP TABLE IF EXISTS fp_source CASCADE;
CREATE TABLE fp_source (id INT PRIMARY KEY, new_val TEXT);
INSERT INTO fp_source VALUES (1, 'from_src');

UPDATE fp_test SET val = fp_source.new_val FROM fp_source WHERE fp_test.id = fp_source.id;
SELECT 'fallback_from' AS test_name,
       (SELECT val FROM fp_test WHERE id = 1) = 'from_src' AS ok;

-- ============================================================
-- Test 10: Disqualifier — no WHERE (update all rows, must fallback)
-- ============================================================
UPDATE fp_test SET val = 'all';
SELECT 'fallback_no_where' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'all') = 5 AS ok;

-- ============================================================
-- Test 11: Disqualifier — range predicate (must fallback)
-- ============================================================
UPDATE fp_test SET val = 'range' WHERE id > 3;
SELECT 'fallback_range' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'range') = 2 AS ok;

-- ============================================================
-- Test 12: Disqualifier — OR predicate (must fallback)
-- ============================================================
UPDATE fp_test SET val = 'or_test' WHERE id = 1 OR id = 2;
SELECT 'fallback_or' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'or_test') = 2 AS ok;

-- ============================================================
-- Test 13: pk = const with RETURNING
-- Fast path must work correctly with RETURNING clause.
-- ============================================================
UPDATE fp_test SET val = 'ret_test' WHERE id = 3 RETURNING id, val;

-- ============================================================
-- Test 14: pk = NULL (equality with NULL, must fallback)
-- col = NULL is UNKNOWN, fast path disqualified.
-- ============================================================
UPDATE fp_test SET val = 'null_eq' WHERE id = NULL;
SELECT 'fallback_pk_null' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'null_eq') = 0 AS ok;

-- ============================================================
-- Test 15: Non-existent PK (fast path returns empty, 0 rows updated)
-- ============================================================
UPDATE fp_test SET val = 'ghost' WHERE id = 999;
SELECT 'pk_nonexistent' AS test_name,
       (SELECT count(*) FROM fp_test WHERE val = 'ghost') = 0 AS ok;

-- ============================================================
-- Test 16: Quoted case-sensitive columns — must not conflate "A" and "a"
-- Regression: to_lowercase() folding collapsed distinct quoted columns
-- into the same key, causing UPDATE 0 instead of UPDATE 1.
-- ============================================================
DROP TABLE IF EXISTS fp_case_bug CASCADE;
CREATE TABLE fp_case_bug (
    "a" INT PRIMARY KEY,
    "A" INT,
    v TEXT
);
INSERT INTO fp_case_bug VALUES (1, 100, 'orig');

UPDATE fp_case_bug SET v = 'hit' WHERE "a" = 1;
SELECT 'quoted_case_pk' AS test_name,
       (SELECT v FROM fp_case_bug WHERE "a" = 1) = 'hit' AS ok;

-- ============================================================
-- Cleanup
-- ============================================================
DROP TABLE fp_test;
DROP TABLE fp_composite;
DROP TABLE fp_unique;
DROP TABLE fp_multi_unique;
DROP TABLE fp_source;
DROP TABLE fp_case_bug;

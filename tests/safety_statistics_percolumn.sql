-- Safety regression tests for per-column statistics storage (PR #2040).
-- Validates ANALYZE works correctly with per-column storage format and
-- WIDTH_THRESHOLD=1024 for wide columns.

-- ================================================================
-- Test 1: Basic ANALYZE + EXPLAIN shows real row counts
-- ================================================================
DROP TABLE IF EXISTS stats_basic;
CREATE TABLE stats_basic (id INT PRIMARY KEY, name TEXT, val INT);
INSERT INTO stats_basic SELECT g, 'row_' || g, g FROM generate_series(1, 100) g;
ANALYZE stats_basic;
EXPLAIN SELECT * FROM stats_basic;

-- ================================================================
-- Test 2: Wide column values don't break ANALYZE (WIDTH_THRESHOLD)
-- ================================================================
DROP TABLE IF EXISTS stats_wide;
CREATE TABLE stats_wide (id INT PRIMARY KEY, payload TEXT);
INSERT INTO stats_wide SELECT g, repeat('x', 2000) FROM generate_series(1, 50) g;
ANALYZE stats_wide;
EXPLAIN SELECT * FROM stats_wide;

-- ================================================================
-- Test 3: Re-ANALYZE after schema change refreshes stats
-- ================================================================
DROP TABLE IF EXISTS stats_reanalyze;
CREATE TABLE stats_reanalyze (id INT PRIMARY KEY, a INT, b INT);
INSERT INTO stats_reanalyze SELECT g, g, g FROM generate_series(1, 200) g;
ANALYZE stats_reanalyze;
EXPLAIN SELECT * FROM stats_reanalyze;
ALTER TABLE stats_reanalyze ADD COLUMN c INT;
-- Stats invalidated by schema change, re-analyze to get fresh stats
ANALYZE stats_reanalyze;
EXPLAIN SELECT * FROM stats_reanalyze;

-- ================================================================
-- Test 4: ANALYZE on empty table
-- ================================================================
DROP TABLE IF EXISTS stats_empty;
CREATE TABLE stats_empty (id INT PRIMARY KEY, val INT);
ANALYZE stats_empty;
EXPLAIN SELECT * FROM stats_empty;

-- ================================================================
-- Test 5: Multi-column table with mixed widths
-- ================================================================
DROP TABLE IF EXISTS stats_mixed;
CREATE TABLE stats_mixed (
    id INT PRIMARY KEY,
    narrow_col INT,
    wide_col TEXT,
    another_narrow INT
);
INSERT INTO stats_mixed
SELECT g, g, repeat('w', 1500), g * 2
FROM generate_series(1, 80) g;
ANALYZE stats_mixed;
EXPLAIN SELECT * FROM stats_mixed;

-- ================================================================
-- Cleanup
-- ================================================================
DROP TABLE IF EXISTS stats_basic;
DROP TABLE IF EXISTS stats_wide;
DROP TABLE IF EXISTS stats_reanalyze;
DROP TABLE IF EXISTS stats_empty;
DROP TABLE IF EXISTS stats_mixed;

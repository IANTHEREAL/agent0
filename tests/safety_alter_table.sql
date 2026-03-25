-- Safety regression tests for ALTER TABLE (PR #2044).
-- Tests DROP COLUMN dependency checks: index expressions, partial index predicates,
-- generated columns, foreign keys, views, CHECK constraints.

-- ================================================================
-- Setup
-- ================================================================

-- ================================================================
-- Test 1: DROP COLUMN blocked by CHECK constraint
-- ================================================================
CREATE TABLE at_dep_check (id INT PRIMARY KEY, age INT CHECK (age > 0));
ALTER TABLE at_dep_check DROP COLUMN age;
DROP TABLE at_dep_check;

-- ================================================================
-- Test 2: DROP COLUMN blocked by expression index
-- ================================================================
CREATE TABLE at_dep_expr_idx (id INT PRIMARY KEY, name TEXT);
CREATE INDEX idx_lower_name ON at_dep_expr_idx ((lower(name)));
ALTER TABLE at_dep_expr_idx DROP COLUMN name;
DROP INDEX idx_lower_name;
DROP TABLE at_dep_expr_idx;

-- ================================================================
-- Test 3: DROP COLUMN blocked by partial index predicate
-- ================================================================
CREATE TABLE at_dep_partial (id INT PRIMARY KEY, active BOOLEAN, val INT);
CREATE INDEX idx_val_active ON at_dep_partial (val) WHERE active = true;
ALTER TABLE at_dep_partial DROP COLUMN active;
DROP INDEX idx_val_active;
DROP TABLE at_dep_partial;

-- ================================================================
-- Test 4: DROP COLUMN blocked by generated column
-- ================================================================
CREATE TABLE at_dep_gen (id INT PRIMARY KEY, a INT, b INT GENERATED ALWAYS AS (a * 2) STORED);
ALTER TABLE at_dep_gen DROP COLUMN a;
DROP TABLE at_dep_gen;

-- ================================================================
-- Test 5: DROP COLUMN blocked by view dependency
-- ================================================================
CREATE TABLE at_dep_view (id INT PRIMARY KEY, name TEXT);
CREATE VIEW at_dep_vw AS SELECT id, name FROM at_dep_view;
ALTER TABLE at_dep_view DROP COLUMN name;
DROP VIEW at_dep_vw;
DROP TABLE at_dep_view;

-- ================================================================
-- Test 6: DROP COLUMN IF EXISTS on nonexistent column (no-op)
-- ================================================================
CREATE TABLE at_dep_noop (id INT PRIMARY KEY, val INT);
ALTER TABLE at_dep_noop DROP COLUMN IF EXISTS nonexistent;
SELECT * FROM at_dep_noop;
DROP TABLE at_dep_noop;

-- ================================================================
-- Test 7: DROP COLUMN succeeds when no dependencies
-- ================================================================
CREATE TABLE at_dep_ok (id INT PRIMARY KEY, a INT, b INT);
INSERT INTO at_dep_ok VALUES (1, 10, 20);
ALTER TABLE at_dep_ok DROP COLUMN b;
SELECT id, a FROM at_dep_ok;
DROP TABLE at_dep_ok;

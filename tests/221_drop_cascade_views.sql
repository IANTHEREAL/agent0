-- Regression: DROP TABLE/VIEW ... CASCADE must drop dependent views
-- transitively and must NOT drop unrelated views.
-- Covers issues #627 (false-positive substring matching) and
-- #628 (non-transitive CASCADE).

-- Cleanup from prior runs.
DROP VIEW IF EXISTS cascade_v3 CASCADE;
DROP VIEW IF EXISTS cascade_v2 CASCADE;
DROP VIEW IF EXISTS cascade_v1 CASCADE;
DROP VIEW IF EXISTS cascade_unrelated CASCADE;
DROP TABLE IF EXISTS cascade_t CASCADE;

-- Setup: table → v1 → v2 (transitive chain).
CREATE TABLE cascade_t (id INT, name TEXT);
INSERT INTO cascade_t VALUES (1, 'a');

CREATE VIEW cascade_v1 AS SELECT * FROM cascade_t;
CREATE VIEW cascade_v2 AS SELECT * FROM cascade_v1;

-- An unrelated view that must survive the CASCADE.
CREATE VIEW cascade_unrelated AS SELECT 42 AS answer;

-- Verify all four objects exist before CASCADE.
SELECT 'BEFORE_TABLE=' || count(*) FROM pg_catalog.pg_tables
  WHERE tablename = 'cascade_t';
SELECT 'BEFORE_V1=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_v1';
SELECT 'BEFORE_V2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_v2';
SELECT 'BEFORE_UNREL=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_unrelated';

-- Drop the base table with CASCADE.
DROP TABLE cascade_t CASCADE;

-- v1 and v2 must be gone (transitive cascade).
SELECT 'AFTER_V1=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_v1';
SELECT 'AFTER_V2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_v2';

-- The unrelated view must survive.
SELECT 'AFTER_UNREL=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade_unrelated';

-- Verify the unrelated view still works.
SELECT * FROM cascade_unrelated;

-- Cleanup.
DROP VIEW cascade_unrelated;

-- ---------------------------------------------------------------
-- Test 2: DROP VIEW ... CASCADE on a mid-chain view.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS cascade2_v3 CASCADE;
DROP VIEW IF EXISTS cascade2_v2 CASCADE;
DROP VIEW IF EXISTS cascade2_v1 CASCADE;
DROP TABLE IF EXISTS cascade2_t CASCADE;

CREATE TABLE cascade2_t (x INT);
CREATE VIEW cascade2_v1 AS SELECT x FROM cascade2_t;
CREATE VIEW cascade2_v2 AS SELECT x FROM cascade2_v1;
CREATE VIEW cascade2_v3 AS SELECT x FROM cascade2_v2;

-- Drop the middle view; v3 must cascade-drop, v1 and table survive.
DROP VIEW cascade2_v1 CASCADE;

SELECT 'MID_V2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade2_v2';
SELECT 'MID_V3=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cascade2_v3';
SELECT 'MID_TABLE=' || count(*) FROM pg_catalog.pg_tables
  WHERE tablename = 'cascade2_t';

-- Cleanup.
DROP TABLE cascade2_t;

-- ---------------------------------------------------------------
-- Test 3: Short table name must not false-positive.
-- Issue #627: table named "t" used to match inside any SQL text.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS short_v_unrelated CASCADE;
DROP TABLE IF EXISTS short_t CASCADE;
DROP TABLE IF EXISTS short_other CASCADE;

CREATE TABLE short_t (id INT);
CREATE TABLE short_other (id INT);
CREATE VIEW short_v_unrelated AS SELECT * FROM short_other;

-- DROP the short-named table; the view on short_other must survive.
DROP TABLE short_t CASCADE;

SELECT 'SHORT_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'short_v_unrelated';

-- Verify it still works.
SELECT * FROM short_v_unrelated;

-- Cleanup.
DROP VIEW short_v_unrelated;
DROP TABLE short_other;

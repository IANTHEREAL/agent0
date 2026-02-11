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

-- ---------------------------------------------------------------
-- Test 4: Dependency cycles must not cause "does not exist" error.
-- Issue #639: drop_dependent_views could drop the target itself
-- via a cycle, then execute_drop_view would fail.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS cycle_v2 CASCADE;
DROP VIEW IF EXISTS cycle_v1 CASCADE;

-- tipg does not validate view references at CREATE time, so
-- cycles can be persisted.
CREATE VIEW cycle_v1 AS SELECT * FROM cycle_v2;
CREATE VIEW cycle_v2 AS SELECT * FROM cycle_v1;

-- Must succeed (not error with "does not exist").
DROP VIEW cycle_v1 CASCADE;

SELECT 'CYCLE_V1=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cycle_v1';
SELECT 'CYCLE_V2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cycle_v2';

-- ---------------------------------------------------------------
-- Test 5: CTE shadowing must not cause false-positive drops.
-- Issue #638: visit_relations reports CTE aliases as table refs.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS cte_v CASCADE;
DROP TABLE IF EXISTS cte_orders CASCADE;

CREATE TABLE cte_orders (id INT);
INSERT INTO cte_orders VALUES (1);
CREATE VIEW cte_v AS WITH cte_orders AS (SELECT 42 AS id) SELECT * FROM cte_orders;

-- The view reads the CTE, not the table — must survive CASCADE.
DROP TABLE cte_orders CASCADE;

SELECT 'CTE_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cte_v';

-- Verify it still works (returns CTE value, not table data).
SELECT * FROM cte_v;

-- Cleanup.
DROP VIEW cte_v;

-- ---------------------------------------------------------------
-- Test 6: Multi-name DROP VIEW CASCADE where a later name is a
-- dependent of an earlier name.
-- Issue #640: the dependent was dropped during CASCADE of the
-- first name, then the loop errored with "does not exist".
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS multi_v2 CASCADE;
DROP VIEW IF EXISTS multi_v1 CASCADE;

CREATE VIEW multi_v1 AS SELECT 1 AS a;
CREATE VIEW multi_v2 AS SELECT * FROM multi_v1;

-- Must succeed: v2 is dropped as a dependent of v1, then skipped.
DROP VIEW multi_v1, multi_v2 CASCADE;

SELECT 'MULTI_V1=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'multi_v1';
SELECT 'MULTI_V2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'multi_v2';

-- ---------------------------------------------------------------
-- Test 7: Self-referential view with CASCADE.
-- Issue #640: drop_dependent_views would drop the target itself
-- (via self-reference), causing the outer drop to fail.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS self_v CASCADE;

CREATE VIEW self_v AS SELECT * FROM self_v;

-- Must succeed.
DROP VIEW self_v CASCADE;

SELECT 'SELF_V=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'self_v';

-- ---------------------------------------------------------------
-- Test 8: Multi-name DROP MATERIALIZED VIEW CASCADE.
-- Issue #640: same pattern as views but for materialized views.
-- ---------------------------------------------------------------
DROP MATERIALIZED VIEW IF EXISTS multi_mv2 CASCADE;
DROP MATERIALIZED VIEW IF EXISTS multi_mv1 CASCADE;

CREATE MATERIALIZED VIEW multi_mv1 AS SELECT 1 AS a;
CREATE MATERIALIZED VIEW multi_mv2 AS SELECT a FROM multi_mv1;

-- Must succeed: mv2 is dropped as a dependent of mv1, then skipped.
DROP MATERIALIZED VIEW multi_mv1, multi_mv2 CASCADE;

SELECT 'MULTI_MV1=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'multi_mv1';
SELECT 'MULTI_MV2=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'multi_mv2';

-- ---------------------------------------------------------------
-- Test 9: Recursive CTE self-reference must not cause false drop.
-- Issue #643: WITH RECURSIVE t AS (... FROM t ...) — the FROM t
-- is the CTE working table, not the real table.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS rc_v CASCADE;
DROP TABLE IF EXISTS rc_t CASCADE;

CREATE TABLE rc_t (n INT);
INSERT INTO rc_t VALUES (1);
CREATE VIEW rc_v AS WITH RECURSIVE rc_t AS (
  SELECT 1 AS n UNION ALL SELECT n+1 FROM rc_t WHERE n<10
) SELECT * FROM rc_t;

-- The view uses the CTE, not the table — must survive CASCADE.
DROP TABLE rc_t CASCADE;

SELECT 'RC_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'rc_v';

-- Verify it returns CTE data (1..10), not table data.
SELECT count(*) AS rc_count FROM rc_v;

-- Cleanup.
DROP VIEW rc_v;

-- ---------------------------------------------------------------
-- Test 10: Later CTE definition must not shadow earlier CTE body.
-- Issue #654: WITH a AS (SELECT * FROM t), t AS (SELECT 1) —
-- CTE t is not yet visible when a's body is walked.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS lt_v CASCADE;
DROP TABLE IF EXISTS lt_t CASCADE;

CREATE TABLE lt_t (id INT);
INSERT INTO lt_t VALUES (42);
CREATE VIEW lt_v AS WITH a AS (SELECT * FROM lt_t), lt_t AS (SELECT 1 AS id) SELECT * FROM a;

-- CTE a's body references the real table lt_t — view must be dropped.
DROP TABLE lt_t CASCADE;

SELECT 'LT_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'lt_v';

-- ---------------------------------------------------------------
-- Test 11: Nested WITH reusing CTE name.
-- Issue #644: inner CTE `a` must not affect outer CTE `a`'s deps.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS nw_v CASCADE;
DROP TABLE IF EXISTS nw_t CASCADE;

CREATE TABLE nw_t (id INT);
INSERT INTO nw_t VALUES (99);
CREATE VIEW nw_v AS WITH a AS (SELECT * FROM nw_t)
  SELECT * FROM (WITH a AS (SELECT 1 AS id) SELECT * FROM a) sub;

-- Outer CTE a references real nw_t — view must be dropped.
DROP TABLE nw_t CASCADE;

SELECT 'NW_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'nw_v';

-- ---------------------------------------------------------------
-- Test 12: Cross-schema search_path dependency.
-- Issue #653: unqualified FROM resolving via search_path to a
-- table in another schema.
-- ---------------------------------------------------------------
DROP VIEW IF EXISTS cs_v CASCADE;
DROP TABLE IF EXISTS cs_s1.cs_t CASCADE;
DROP SCHEMA IF EXISTS cs_s1 CASCADE;

CREATE SCHEMA cs_s1;
CREATE TABLE cs_s1.cs_t (id INT);
INSERT INTO cs_s1.cs_t VALUES (7);
SET search_path = cs_s1, public;
CREATE VIEW cs_v AS SELECT * FROM cs_t;
SET search_path = public;

-- The view depends on cs_s1.cs_t via search_path — must be dropped.
DROP TABLE cs_s1.cs_t CASCADE;

SELECT 'CS_VIEW=' || count(*) FROM pg_catalog.pg_views
  WHERE viewname = 'cs_v';

-- Cleanup.
DROP SCHEMA IF EXISTS cs_s1 CASCADE;

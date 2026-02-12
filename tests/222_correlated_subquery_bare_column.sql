-- Issue #548 / #581: Correlated scalar subqueries with bare column references.
-- Bare (unqualified) columns inside a subquery that don't exist in the inner
-- table's scope should resolve to the outer table, following PostgreSQL's
-- name-resolution rules.  Inner-scope columns must shadow outer ones.

DROP TABLE IF EXISTS csb_outer;
DROP TABLE IF EXISTS csb_inner;
DROP TABLE IF EXISTS csb_both;

CREATE TABLE csb_outer (a INT, b INT);
CREATE TABLE csb_inner (c INT, d INT);
CREATE TABLE csb_both  (a INT, val TEXT);

INSERT INTO csb_outer VALUES (1, 10), (2, 20), (3, 30);
INSERT INTO csb_inner VALUES (100, 1000);
INSERT INTO csb_both  VALUES (1, 'x'), (2, 'y');

-- Test 1: Basic bare outer column reference (#548 repro)
-- csb_outer has (a, b); csb_inner has (c, d) — no overlap.
-- Bare `a` inside the subquery must resolve to csb_outer.a.
SELECT a, (SELECT a + c FROM csb_inner) AS sub_val FROM csb_outer ORDER BY a;

-- Test 2: Inner scope shadowing (#581 repro)
-- csb_both has column `a` — same name as csb_outer.a.
-- Bare `a` inside the subquery must resolve to csb_both.a (inner scope wins).
SELECT a AS outer_a,
       (SELECT a FROM csb_both WHERE csb_both.a = csb_outer.a) AS inner_a
FROM csb_outer
WHERE a <= 2
ORDER BY outer_a;

-- Test 3: Uncorrelated subquery must NOT be misdetected
-- All columns in the subquery belong to csb_inner; no outer reference.
SELECT a, (SELECT c FROM csb_inner LIMIT 1) AS uncorr FROM csb_outer ORDER BY a;

-- Test 4: Mixed qualified and bare references
-- Qualified csb_outer.a works as before; bare c is inner.
SELECT a, (SELECT csb_outer.a + c FROM csb_inner) AS mixed FROM csb_outer ORDER BY a;

-- Test 5: Bare outer column in WHERE of subquery
SELECT a, (SELECT d FROM csb_inner WHERE c > a) AS filtered FROM csb_outer ORDER BY a;

DROP TABLE csb_outer;
DROP TABLE csb_inner;
DROP TABLE csb_both;

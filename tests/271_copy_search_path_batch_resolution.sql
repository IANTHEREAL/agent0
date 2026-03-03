-- Test: COPY FROM STDIN unqualified table resolution via batch fetch.
-- Covers: search_path priority, duplicate entries, fallback to second schema.

-- ============================================================
-- Setup
-- ============================================================
DROP TABLE IF EXISTS csp_s1.t_batch;
DROP TABLE IF EXISTS csp_s2.t_batch;
DROP TABLE IF EXISTS public.t_batch;
DROP SCHEMA IF EXISTS csp_s1;
DROP SCHEMA IF EXISTS csp_s2;
CREATE SCHEMA csp_s1;
CREATE SCHEMA csp_s2;

CREATE TABLE csp_s1.t_batch (id INT, tag TEXT);
CREATE TABLE csp_s2.t_batch (id INT, tag TEXT);

-- ============================================================
-- 1. search_path resolves to first schema containing the table
-- ============================================================
SET search_path TO csp_s1, csp_s2;

COPY t_batch (id, tag) FROM STDIN;
1	first
\.

SELECT id, tag FROM csp_s1.t_batch ORDER BY id;
SELECT COUNT(*) FROM csp_s2.t_batch;

-- ============================================================
-- 2. Duplicate search_path entries still resolve correctly
-- ============================================================
SET search_path TO csp_s1, csp_s1, csp_s2;

COPY t_batch (id, tag) FROM STDIN;
2	dup_path
\.

SELECT id, tag FROM csp_s1.t_batch ORDER BY id;
SELECT COUNT(*) FROM csp_s2.t_batch;

-- ============================================================
-- 3. Fallback: table only in second schema
-- ============================================================
DROP TABLE csp_s1.t_batch;
SET search_path TO csp_s1, csp_s2;

COPY t_batch (id, tag) FROM STDIN;
3	second_schema
\.

SELECT id, tag FROM csp_s2.t_batch ORDER BY id;

-- ============================================================
-- 4. Quoted/case-sensitive search_path entry
-- ============================================================
CREATE SCHEMA "MySchema";
CREATE TABLE "MySchema".t_batch (id INT, tag TEXT);
SET search_path TO "MySchema", public;

COPY t_batch (id, tag) FROM STDIN;
4	case_sensitive
\.

SELECT id, tag FROM "MySchema".t_batch ORDER BY id;

DROP TABLE "MySchema".t_batch;
DROP SCHEMA "MySchema";

-- ============================================================
-- 5. Not found → 42P01 (at end to keep output clean)
-- ============================================================
SET search_path TO csp_s1;

COPY t_batch_missing (id) FROM STDIN;

-- ============================================================
-- Cleanup
-- ============================================================
DROP TABLE IF EXISTS csp_s2.t_batch;
DROP SCHEMA IF EXISTS csp_s1;
DROP SCHEMA IF EXISTS csp_s2;

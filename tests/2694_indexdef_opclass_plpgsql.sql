-- #2694: pg_get_indexdef() must keep the operator class on the PL/pgSQL /
-- sequence-expr evaluation path. That path (src/sql/sequences/index_helpers.rs,
-- reached via the PG_GET_INDEXDEF dispatch during user-function evaluation) used
-- a SECOND, duplicate index-def formatter that dropped the operator class, while
-- the analyzed-SELECT / pg_indexes path (src/sql/catalog/helpers.rs) was fixed in
-- #2684. So `SELECT pg_get_indexdef(...)` was correct, but the SAME call inside a
-- PL/pgSQL function body silently dropped `varchar_pattern_ops`, breaking
-- PL/pgSQL-based schema/migration tooling. The fix routes both paths through one
-- formatter (catalog::helpers::format_indexdef), so they can never diverge again.
--
-- Output is lower()-normalized: db9 emits pg_get_indexdef keywords in lowercase
-- (a separate, pre-existing divergence from PostgreSQL's uppercase); the
-- operator-class token itself is identical to PostgreSQL 17.
SET client_min_messages = warning;

DROP TABLE IF EXISTS t2694 CASCADE;
CREATE TABLE t2694 (id INT PRIMARY KEY, session_key VARCHAR(40), name TEXT);
CREATE INDEX t2694_session_like ON t2694 (session_key varchar_pattern_ops);
CREATE INDEX t2694_name_like    ON t2694 (name text_pattern_ops);

-- A PL/pgSQL function body invokes pg_get_indexdef -> exercises the
-- sequence-expr evaluation path (unlike a plain top-level SELECT, which uses the
-- analyzed-SELECT path and was already correct).
CREATE OR REPLACE FUNCTION idxdef_2694(idx_oid oid) RETURNS text
  LANGUAGE plpgsql AS $$
BEGIN
  RETURN pg_get_indexdef(idx_oid);
END;
$$;

-- The operator class must survive the PL/pgSQL path (this is the #2694 fix).
SELECT 'PLPGSQL_IDXDEF=' || lower(idxdef_2694(c.oid))
FROM pg_class c
WHERE c.relname IN ('t2694_session_like', 't2694_name_like')
ORDER BY c.relname;

DROP TABLE t2694;

-- PG_PARITY: pg_index.indclass reflects the requested operator class like PostgreSQL.
-- #2695: a non-default operator class must be reflected in pg_index.indclass, so
-- catalog introspection resolves varchar_pattern_ops / text_pattern_ops (via
-- pg_opclass) rather than the column's default opclass. #2685 made CREATE INDEX
-- accept and persist these; this locks the catalog round-trip.
--
-- The query unnests indclass (cast to oid[]) and joins pg_opclass — it runs
-- identically on PG 17 and db9. NOTE: the lateral table-function form
-- `unnest(indclass) WITH ORDINALITY` that ORM reflection (SQLAlchemy) can use is
-- not yet supported by db9 (unnest of oidvector as a table function, and
-- WITH ORDINALITY, are tracked in #2710); this scalar-subquery unnest is the
-- portable subset and asserts the same catalog content. Verified vs PostgreSQL 17.
SET client_min_messages = warning;

DROP TABLE IF EXISTS t2695 CASCADE;
CREATE TABLE t2695 (id INT PRIMARY KEY, session_key VARCHAR(40), name TEXT);
CREATE INDEX t2695_session_like ON t2695 (session_key varchar_pattern_ops);
CREATE INDEX t2695_name_like ON t2695 (name text_pattern_ops);

-- Unnest pg_index.indclass and join pg_opclass.oid, asserting each index
-- reflects its requested (non-default) operator class.
SELECT 'INDCLASS=' || c.relname || ':' || o.opcname AS reflected
FROM pg_index i
JOIN pg_class c ON c.oid = i.indexrelid
JOIN pg_opclass o ON o.oid IN (SELECT unnest(i.indclass::oid[]))
WHERE c.relname IN ('t2695_session_like', 't2695_name_like')
ORDER BY c.relname, o.opcname;

DROP TABLE t2695 CASCADE;

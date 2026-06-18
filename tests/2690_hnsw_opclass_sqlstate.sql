-- PG_PARITY: HNSW opclass validation SQLSTATEs match PostgreSQL (42804/42704).
-- #2690: HNSW operator-class validation must surface PostgreSQL SQLSTATEs, not
-- an internal XX000.
--
-- PostgreSQL's CREATE INDEX operator-class resolution is access-method
-- independent: an operator class that does not accept the column's type is
-- 42804 (operator class does not accept data type), and a column type with no
-- default operator class for the access method is 42704 (no default operator
-- class). db9 previously returned both as untyped errors on the HNSW path, so
-- the wire SQLSTATE was XX000 instead of the PostgreSQL code — drivers/ORMs that
-- branch on SQLSTATE could not tell a user error from an internal failure.
--
-- SQLSTATE/message parity verified on real PostgreSQL 17 via the access-method
-- independent btree equivalents:
--   "operator class X does not accept data type Y"             -> 42804
--   "data type X has no default operator class for access ..." -> 42704
-- pgvector is not required to validate the codes: they come from PostgreSQL
-- core opclass resolution, not the extension.
\set VERBOSITY verbose
SET client_min_messages = warning;

DROP TABLE IF EXISTS t2690 CASCADE;
CREATE TABLE t2690 (id INT PRIMARY KEY, embedding VECTOR(3), label TEXT, code VARCHAR(16));

-- Explicit vector operator class on a non-vector (text) column -> 42804.
CREATE INDEX t2690_text_cosine ON t2690 USING hnsw (label vector_cosine_ops);

-- Explicit vector operator class on a varchar column -> 42804; the type renders
-- PG-style as "character varying" (not "varchar(16)").
CREATE INDEX t2690_varchar_l2 ON t2690 USING hnsw (code vector_l2_ops);

-- A bare HNSW index on a non-vector column has no default operator class -> 42704.
CREATE INDEX t2690_text_nodefault ON t2690 USING hnsw (label);

DROP TABLE t2690 CASCADE;

-- No-PK table: opclass/type error must fire BEFORE the db9-specific
-- "single-column PK" requirement (i.e., 42804/42704, not XX000).
DROP TABLE IF EXISTS t2690_nopk CASCADE;
CREATE TABLE t2690_nopk (label TEXT);

-- Explicit vector opclass on a non-vector column of a no-PK table -> 42804.
CREATE INDEX ON t2690_nopk USING hnsw (label vector_cosine_ops);

-- No opclass on a non-vector column of a no-PK table -> 42704.
CREATE INDEX ON t2690_nopk USING hnsw (label);

DROP TABLE t2690_nopk CASCADE;

-- Composite-PK table: same ordering requirement applies.
DROP TABLE IF EXISTS t2690_cpk CASCADE;
CREATE TABLE t2690_cpk (a INT, b INT, label TEXT, PRIMARY KEY (a, b));

-- Explicit vector opclass on a non-vector column of a composite-PK table -> 42804.
CREATE INDEX ON t2690_cpk USING hnsw (label vector_cosine_ops);

-- No opclass on a non-vector column of a composite-PK table -> 42704.
CREATE INDEX ON t2690_cpk USING hnsw (label);

DROP TABLE t2690_cpk CASCADE;

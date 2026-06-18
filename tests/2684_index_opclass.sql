-- #2684: operator-class qualifiers in CREATE INDEX (varchar/text pattern_ops).
-- Django creates a companion *_like index for every indexed text column using
-- these opclasses; the parser must accept them, build a usable index, AND the
-- catalog must report the requested opclass so ORM introspection round-trips
-- (otherwise Django loops re-emitting the same migration). Unknown opclasses
-- must be rejected exactly like PostgreSQL (SQLSTATE 42704).
--
-- NOTE on bpchar: db9 maps CHAR(n) to VARCHAR (it has no distinct bpchar type),
-- so a char column is varchar-typed. The btree text-family opclass check models
-- PG's binary coercibility for text<->varchar only; bpchar_pattern_ops (input
-- type bpchar) is therefore NOT accepted on text/varchar columns, matching
-- PG 17. (A faithful char-column vs. bpchar_pattern_ops round-trip needs a
-- distinct bpchar type in db9 — tracked separately.)
SET client_min_messages = warning;

DROP TABLE IF EXISTS t2684 CASCADE;
CREATE TABLE t2684 (
  id INT PRIMARY KEY,
  session_key VARCHAR(40),
  name TEXT
);
INSERT INTO t2684 VALUES
  (1, 'abc123', 'alpha'),
  (2, 'abd456', 'beta'),
  (3, 'xyz789', 'gamma');

-- Per-opclass index creation (the repro from #2684).
CREATE INDEX t2684_session_like ON t2684 (session_key varchar_pattern_ops);
CREATE INDEX t2684_name_like    ON t2684 (name text_pattern_ops);

-- Opclass combined with ordering options is also accepted.
CREATE INDEX t2684_name_like_desc ON t2684 (name text_pattern_ops DESC NULLS LAST);

-- A quoted opclass name that case-folds (per PG identifier rules) to a known
-- bare opclass is accepted — quoting an already-lowercase name is a no-op.
CREATE INDEX t2684_name_quoted ON t2684 (name "text_pattern_ops");

-- A correctly schema-qualified builtin opclass is accepted: text_pattern_ops
-- lives in pg_catalog, so pg_catalog.text_pattern_ops resolves. PostgreSQL
-- strips the schema and prints the BARE name in pg_get_indexdef / indexdef.
CREATE INDEX t2684_name_pgcat ON t2684 (name pg_catalog.text_pattern_ops);

-- Indexes are registered in the catalog.
SELECT indexname
FROM pg_indexes
WHERE tablename = 't2684'
ORDER BY indexname;

-- Catalog introspection round-trips the requested opclass: pg_indexes.indexdef
-- reflects *_pattern_ops. The pg_catalog-qualified index prints the BARE
-- opclass name (schema stripped), matching PostgreSQL.
SELECT 'IDXDEF=' || indexname || ':' || lower(indexdef)
FROM pg_indexes
WHERE tablename = 't2684' AND indexname IN ('t2684_name_pgcat', 't2684_session_like')
ORDER BY indexname;

-- pg_get_indexdef reports the same opclass.
SELECT 'PG_GET_INDEXDEF=' || lower(pg_get_indexdef(c.oid))
FROM pg_class c
WHERE c.relname = 't2684_name_like';

-- The indexes are usable: prefix / equality queries return correct rows.
SELECT id, session_key FROM t2684 WHERE session_key LIKE 'ab%' ORDER BY id;
SELECT id, name FROM t2684 WHERE name = 'beta' ORDER BY id;

-- An unknown operator class is rejected, exactly like PostgreSQL (42704).
CREATE INDEX t2684_bogus ON t2684 (name made_up_ops);

-- Input-type mismatch: an opclass is only valid for its declared input type.
-- text_pattern_ops accepts text-family columns, not integer (SQLSTATE 42804).
CREATE INDEX t2684_badtype ON t2684 (id text_pattern_ops);

-- A non-default opclass for an incompatible type is also rejected (42804):
-- int8_ops (default for bigint) does not accept integer.
CREATE INDEX t2684_badtype2 ON t2684 (id int8_ops);

-- bpchar_pattern_ops has input type bpchar, which is NOT binary-coercible to
-- text/varchar; applying it to a text column is rejected with 42804, exactly
-- like PostgreSQL 17 (the btree text family is text<->varchar only, not bpchar).
CREATE INDEX t2684_bpchar_bad ON t2684 (name bpchar_pattern_ops);

-- A schema-qualified opclass under the WRONG schema does not resolve: the
-- builtin opclasses live in pg_catalog, not public. PostgreSQL rejects with 42704.
CREATE INDEX t2684_qualified ON t2684 (name public.text_pattern_ops);

-- A quoted, mixed-case opclass name is case-preserving and therefore does NOT
-- match the lowercase bare opclass; PostgreSQL rejects it with 42704.
CREATE INDEX t2684_quoted_bad ON t2684 (name "Text_Pattern_Ops");

-- Operator-class parameters on an opclass that takes none are rejected (22023).
CREATE INDEX t2684_opts ON t2684 (name text_pattern_ops (siglen = 100));

-- Expression-index elements get the SAME opclass input-type check as columns:
-- (id + 1) is integer, which text_pattern_ops (text-family) does not accept (42804).
CREATE INDEX t2684_expr_badtype ON t2684 ((id + 1) text_pattern_ops);

DROP TABLE t2684;

-- ── HNSW opclass validation routes through the same entry point ─────────────
-- Reference: PostgreSQL 17.x + pgvector. HNSW vector opclasses are validated
-- like btree/gin opclasses — bare-name resolution (42704) and no-options
-- (22023) — during opclass validation, before any index build.
DROP TABLE IF EXISTS v2684 CASCADE;
CREATE TABLE v2684 (
  id INT PRIMARY KEY,
  embedding VECTOR(3)
);

-- Opclass parameters on an HNSW vector opclass (which takes none) are rejected
-- with 22023, instead of being silently ignored.
CREATE INDEX v2684_opts ON v2684 USING hnsw (embedding vector_cosine_ops (ef = 8));

-- An unknown HNSW operator class is rejected with 42704 (UndefinedObject),
-- not an internal XX000 error.
CREATE INDEX v2684_bogus ON v2684 USING hnsw (embedding made_up_ops);

DROP TABLE v2684;

-- HNSW Non-Integer PK Support (Mapped Label Mode)
-- Tests: VARCHAR PK, TEXT PK, UUID PK with HNSW indexes
-- Validates CREATE INDEX, INSERT, DELETE, k-NN search, distance ordering

-- ── 1. VARCHAR PK — basic CRUD + search ──────────────────────────────

DROP TABLE IF EXISTS hnsw_varchar_pk;

CREATE TABLE hnsw_varchar_pk (
    id VARCHAR(64) PRIMARY KEY,
    v VECTOR(3)
);

INSERT INTO hnsw_varchar_pk (id, v) VALUES ('alpha', '[1.0, 0.0, 0.0]');
INSERT INTO hnsw_varchar_pk (id, v) VALUES ('beta',  '[0.0, 1.0, 0.0]');
INSERT INTO hnsw_varchar_pk (id, v) VALUES ('gamma', '[0.0, 0.0, 1.0]');

-- Create index on existing data (backfill with mapping)
CREATE INDEX idx_hnsw_varchar ON hnsw_varchar_pk USING hnsw (v vector_l2_ops);

-- k-NN search should return closest vector
SELECT id AS varchar_nearest FROM hnsw_varchar_pk ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── 2. INSERT after index — delta-log with mapping ───────────────────

INSERT INTO hnsw_varchar_pk (id, v) VALUES ('delta', '[0.9, 0.1, 0.0]');
SELECT id AS varchar_insert_nearest FROM hnsw_varchar_pk ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 2;

-- ── 3. DELETE — rowid mapping cleanup ────────────────────────────────

DELETE FROM hnsw_varchar_pk WHERE id = 'alpha';
SELECT id AS varchar_after_delete FROM hnsw_varchar_pk ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── 4. Distance expression with non-integer PK ──────────────────────

SELECT id, l2_distance(v, '[1.0, 0.0, 0.0]') AS dist
FROM hnsw_varchar_pk
ORDER BY v <-> '[1.0, 0.0, 0.0]'
LIMIT 3;

-- ── 5. TEXT PK — create on empty table then insert ──────────────────

DROP TABLE IF EXISTS hnsw_text_pk;

CREATE TABLE hnsw_text_pk (
    name TEXT PRIMARY KEY,
    embedding VECTOR(3)
);

CREATE INDEX idx_hnsw_text ON hnsw_text_pk USING hnsw (embedding vector_cosine_ops);

INSERT INTO hnsw_text_pk (name, embedding) VALUES ('dog',   '[1.0, 0.0, 0.0]');
INSERT INTO hnsw_text_pk (name, embedding) VALUES ('cat',   '[0.9, 0.1, 0.0]');
INSERT INTO hnsw_text_pk (name, embedding) VALUES ('fish',  '[0.0, 0.0, 1.0]');

SELECT name AS text_cosine_nearest FROM hnsw_text_pk ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── 6. Multiple HNSW indexes on same table share rowid mapping ──────

DROP TABLE IF EXISTS hnsw_multi_idx;

CREATE TABLE hnsw_multi_idx (
    code VARCHAR(32) PRIMARY KEY,
    v1 VECTOR(3),
    v2 VECTOR(3)
);

INSERT INTO hnsw_multi_idx VALUES ('x1', '[1.0, 0.0, 0.0]', '[0.0, 1.0, 0.0]');
INSERT INTO hnsw_multi_idx VALUES ('x2', '[0.0, 1.0, 0.0]', '[1.0, 0.0, 0.0]');
INSERT INTO hnsw_multi_idx VALUES ('x3', '[0.0, 0.0, 1.0]', '[0.0, 0.0, 1.0]');

CREATE INDEX idx_multi_v1 ON hnsw_multi_idx USING hnsw (v1 vector_l2_ops);
CREATE INDEX idx_multi_v2 ON hnsw_multi_idx USING hnsw (v2 vector_l2_ops);

SELECT code AS multi_v1_nearest FROM hnsw_multi_idx ORDER BY v1 <-> '[1.0, 0.0, 0.0]' LIMIT 1;
SELECT code AS multi_v2_nearest FROM hnsw_multi_idx ORDER BY v2 <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── 7. Integer PK still works (backward compat — Direct mode) ───────

DROP TABLE IF EXISTS hnsw_int_pk_compat;

CREATE TABLE hnsw_int_pk_compat (
    id BIGINT PRIMARY KEY,
    v VECTOR(3)
);

INSERT INTO hnsw_int_pk_compat VALUES (100, '[1.0, 0.0, 0.0]');
INSERT INTO hnsw_int_pk_compat VALUES (200, '[0.0, 1.0, 0.0]');

CREATE INDEX idx_int_compat ON hnsw_int_pk_compat USING hnsw (v vector_l2_ops);

SELECT id AS int_compat_nearest FROM hnsw_int_pk_compat ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── 8. UPDATE vector value — ANN still returns correct row ──────────

UPDATE hnsw_varchar_pk SET v = '[0.0, 1.0, 0.0]' WHERE id = 'delta';
SELECT id AS varchar_after_update FROM hnsw_varchar_pk ORDER BY v <-> '[0.0, 1.0, 0.0]' LIMIT 1;

-- ── 9. NULL vector handling with non-integer PK ─────────────────────

INSERT INTO hnsw_varchar_pk (id, v) VALUES ('nullvec', NULL);
SELECT id AS varchar_null_search FROM hnsw_varchar_pk ORDER BY v <-> '[1.0, 0.0, 0.0]' LIMIT 1;

-- ── Cleanup ─────────────────────────────────────────────────────────

DROP TABLE hnsw_varchar_pk;
DROP TABLE hnsw_text_pk;
DROP TABLE hnsw_multi_idx;
DROP TABLE hnsw_int_pk_compat;

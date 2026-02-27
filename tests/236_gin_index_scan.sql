-- GIN Index Scan integration tests
-- Validates that GIN indexes are used for @@, @>, && predicates.
-- Phase 1 regression gate for GIN scan enablement.

-- ============================================================
-- Section 1: Tsvector @@ with column GIN index
-- ============================================================
DROP TABLE IF EXISTS gin_scan_fts;
CREATE TABLE gin_scan_fts (
    id SERIAL PRIMARY KEY,
    doc TSVECTOR
);
CREATE INDEX idx_gin_scan_fts ON gin_scan_fts USING gin (doc);

INSERT INTO gin_scan_fts (doc) VALUES
    (to_tsvector('hello world')),
    (to_tsvector('hello rust programming')),
    (to_tsvector('world database systems')),
    (to_tsvector('rust is fast')),
    (to_tsvector('database indexing techniques'));

-- 1a: Single term AND (plainto_tsquery)
SELECT id FROM gin_scan_fts WHERE doc @@ plainto_tsquery('hello') ORDER BY id;

-- 1b: Multi-term AND
SELECT id FROM gin_scan_fts WHERE doc @@ plainto_tsquery('hello world') ORDER BY id;

-- 1c: OR query
SELECT id FROM gin_scan_fts WHERE doc @@ 'hello | database'::tsquery ORDER BY id;

-- 1d: AND + NOT (difference)
SELECT id FROM gin_scan_fts WHERE doc @@ 'hello & !world'::tsquery ORDER BY id;

-- 1e: Complex boolean (A | B) & C
SELECT id FROM gin_scan_fts WHERE doc @@ '(hello | database) & rust'::tsquery ORDER BY id;

-- 1f: No match
SELECT id FROM gin_scan_fts WHERE doc @@ plainto_tsquery('nonexistent') ORDER BY id;

-- 1g: EXPLAIN should show GIN Index Scan
EXPLAIN SELECT id FROM gin_scan_fts WHERE doc @@ plainto_tsquery('hello');

DROP TABLE gin_scan_fts;

-- ============================================================
-- Section 2: Expression index (to_tsvector('config', col))
-- ============================================================
DROP TABLE IF EXISTS gin_scan_expr;
CREATE TABLE gin_scan_expr (
    id SERIAL PRIMARY KEY,
    body TEXT NOT NULL
);
CREATE INDEX idx_gin_expr ON gin_scan_expr USING gin (to_tsvector('english', body));

INSERT INTO gin_scan_expr (body) VALUES
    ('The quick brown fox jumps over the lazy dog'),
    ('Rust programming language is fast and safe'),
    ('PostgreSQL is a powerful database system'),
    ('Full text search with inverted indexes'),
    ('The dog and the fox are friends');

-- 2a: Expression index match
SELECT id FROM gin_scan_expr
WHERE to_tsvector('english', body) @@ plainto_tsquery('english', 'fox')
ORDER BY id;

-- 2b: Multi-term with expression index
SELECT id FROM gin_scan_expr
WHERE to_tsvector('english', body) @@ plainto_tsquery('english', 'database system')
ORDER BY id;

-- 2c: EXPLAIN should show GIN scan on expression index
EXPLAIN SELECT id FROM gin_scan_expr
WHERE to_tsvector('english', body) @@ plainto_tsquery('english', 'fox');

DROP TABLE gin_scan_expr;

-- ============================================================
-- Section 3: JSONB @> with GIN index
-- ============================================================
DROP TABLE IF EXISTS gin_scan_jsonb;
CREATE TABLE gin_scan_jsonb (
    id SERIAL PRIMARY KEY,
    data JSONB NOT NULL
);
CREATE INDEX idx_gin_jsonb ON gin_scan_jsonb USING gin (data);

INSERT INTO gin_scan_jsonb (data) VALUES
    ('{"color": "red", "size": 10}'),
    ('{"color": "blue", "size": 20}'),
    ('{"color": "red", "size": 30, "tags": ["hot", "sale"]}'),
    ('{"nested": {"key": "val"}}'),
    ('{"color": "green"}');

-- 3a: Simple key-value containment
SELECT id FROM gin_scan_jsonb WHERE data @> '{"color": "red"}' ORDER BY id;

-- 3b: Multi-key containment
SELECT id FROM gin_scan_jsonb WHERE data @> '{"color": "red", "size": 10}' ORDER BY id;

-- 3c: Nested containment
SELECT id FROM gin_scan_jsonb WHERE data @> '{"nested": {"key": "val"}}' ORDER BY id;

-- 3d: No match
SELECT id FROM gin_scan_jsonb WHERE data @> '{"color": "yellow"}' ORDER BY id;

-- 3e: EXPLAIN should show GIN scan for JSONB @>
EXPLAIN SELECT id FROM gin_scan_jsonb WHERE data @> '{"color": "red"}';

DROP TABLE gin_scan_jsonb;

-- ============================================================
-- Section 4: ARRAY @> and && with GIN index
-- ============================================================
DROP TABLE IF EXISTS gin_scan_array;
CREATE TABLE gin_scan_array (
    id SERIAL PRIMARY KEY,
    tags TEXT[]
);
CREATE INDEX idx_gin_array ON gin_scan_array USING gin (tags);

INSERT INTO gin_scan_array (tags) VALUES
    (ARRAY['rust', 'systems', 'fast']),
    (ARRAY['python', 'ml', 'data']),
    (ARRAY['rust', 'web', 'api']),
    (ARRAY['go', 'cloud', 'fast']),
    (ARRAY['python', 'web', 'django']);

-- 4a: Array containment (AND of all elements)
SELECT id FROM gin_scan_array WHERE tags @> ARRAY['rust'] ORDER BY id;

-- 4b: Multi-element containment
SELECT id FROM gin_scan_array WHERE tags @> ARRAY['rust', 'fast'] ORDER BY id;

-- 4c: Array overlap (OR of any element)
SELECT id FROM gin_scan_array WHERE tags && ARRAY['rust', 'python'] ORDER BY id;

-- 4d: No match containment
SELECT id FROM gin_scan_array WHERE tags @> ARRAY['java'] ORDER BY id;

-- 4e: EXPLAIN should show GIN scan for array @>
EXPLAIN SELECT id FROM gin_scan_array WHERE tags @> ARRAY['rust'];

-- 4f: EXPLAIN should show GIN scan for array &&
EXPLAIN SELECT id FROM gin_scan_array WHERE tags && ARRAY['rust', 'python'];

DROP TABLE gin_scan_array;

-- ============================================================
-- Section 5: Chinese FTS with GIN expression index
-- ============================================================
DROP TABLE IF EXISTS gin_scan_chinese;
CREATE TABLE gin_scan_chinese (
    id SERIAL PRIMARY KEY,
    content TEXT NOT NULL
);
CREATE INDEX idx_gin_chinese ON gin_scan_chinese USING gin (to_tsvector('chinese', content));

INSERT INTO gin_scan_chinese (content) VALUES
    ('分布式数据库是现代互联网架构的核心组件'),
    ('全文搜索引擎使用倒排索引来加速查询'),
    ('深度学习是机器学习的一个重要分支');

-- 5a: Chinese query
SELECT id FROM gin_scan_chinese
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '数据库')
ORDER BY id;

-- 5b: EXPLAIN should show GIN scan for Chinese
EXPLAIN SELECT id FROM gin_scan_chinese
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '数据库');

DROP TABLE gin_scan_chinese;

-- ============================================================
-- Section 6: Correctness regression — GIN scan + recheck
-- Ensures no false negatives (candidate set only expands, never shrinks)
-- ============================================================
DROP TABLE IF EXISTS gin_scan_recheck;
CREATE TABLE gin_scan_recheck (
    id SERIAL PRIMARY KEY,
    doc TSVECTOR
);
CREATE INDEX idx_gin_recheck ON gin_scan_recheck USING gin (doc);

-- Insert docs where tokens are a superset of query tokens
INSERT INTO gin_scan_recheck (doc) VALUES
    (to_tsvector('alpha beta gamma')),
    (to_tsvector('alpha delta epsilon')),
    (to_tsvector('beta gamma delta')),
    (to_tsvector('alpha beta delta gamma'));

-- 6a: AND — both tokens present
SELECT id FROM gin_scan_recheck WHERE doc @@ 'alpha & beta'::tsquery ORDER BY id;

-- 6b: OR — either token
SELECT id FROM gin_scan_recheck WHERE doc @@ 'alpha | gamma'::tsquery ORDER BY id;

-- 6c: NOT — alpha but not gamma
SELECT id FROM gin_scan_recheck WHERE doc @@ 'alpha & !gamma'::tsquery ORDER BY id;

-- 6d: Complex — (alpha | epsilon) & delta
SELECT id FROM gin_scan_recheck WHERE doc @@ '(alpha | epsilon) & delta'::tsquery ORDER BY id;

DROP TABLE gin_scan_recheck;
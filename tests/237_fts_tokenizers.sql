-- Phase 2: FTS tokenizer enhancement tests
-- Tests n-gram, english_stem, chinese_ngram tokenizers,
-- and CREATE/DROP TEXT SEARCH CONFIGURATION DDL.

-- ============================================================
-- Section 1: N-gram tokenizer (bigram)
-- ============================================================

-- 1a: CJK bigram tokenization
SELECT to_tsvector('ngram', '数据库技术');

-- 1b: English bigram tokenization
SELECT to_tsvector('ngram', 'database');

-- 1c: N-gram query matching — substring match via bigrams
SELECT to_tsvector('ngram', '数据库技术') @@ plainto_tsquery('ngram', '数据');

-- 1d: N-gram no-match
SELECT to_tsvector('ngram', '数据库技术') @@ plainto_tsquery('ngram', '人工智能');

-- ============================================================
-- Section 2: Trigram tokenizer
-- ============================================================

-- 2a: CJK trigram
SELECT to_tsvector('trigram', '数据库技术');

-- 2b: Short segment (< 3 chars) emitted as-is
SELECT to_tsvector('trigram', '数据');

-- ============================================================
-- Section 3: English stemmed tokenizer
-- ============================================================

-- 3a: Stemming reduces words to their root form
-- "running" → "run", "dogs" → "dog", "happy" → "happi"
-- Stopwords ("the", "are") are filtered out in tsvector output
SELECT to_tsvector('english_stem', 'The running dogs are happy');

-- 3b: Stemmed query matches stemmed document
SELECT to_tsvector('english_stem', 'The running dogs are happy')
    @@ plainto_tsquery('english_stem', 'run dog');

-- 3c: Unstemmed form still matches (tokenizer normalizes both sides)
SELECT to_tsvector('english_stem', 'The running dogs are happy')
    @@ plainto_tsquery('english_stem', 'running dogs');

-- 3d: Stopword-only query produces empty tsquery
SELECT plainto_tsquery('english_stem', 'the a an is');

-- ============================================================
-- Section 4: Chinese ngram (jieba + bigram overlay)
-- ============================================================

-- 4a: Contains both jieba words and bigram overlays
SELECT to_tsvector('chinese_ngram', '分布式数据库');

-- 4b: Substring match via bigram overlay
SELECT to_tsvector('chinese_ngram', '分布式数据库')
    @@ plainto_tsquery('chinese_ngram', '数据');

-- ============================================================
-- Section 5: GIN index scan with new tokenizer configs
-- ============================================================

DROP TABLE IF EXISTS gin_ngram_test;
CREATE TABLE gin_ngram_test (
    id SERIAL PRIMARY KEY,
    content TEXT
);

INSERT INTO gin_ngram_test (content) VALUES
    ('数据库管理系统'),
    ('人工智能技术'),
    ('数据分析平台'),
    ('云计算服务'),
    ('机器学习算法');

-- Create a GIN index using ngram tokenizer
CREATE INDEX idx_gin_ngram ON gin_ngram_test USING gin (to_tsvector('ngram', content));

-- 5a: GIN scan with ngram — should find rows containing '数据'
SELECT id FROM gin_ngram_test
WHERE to_tsvector('ngram', content) @@ plainto_tsquery('ngram', '数据')
ORDER BY id;

-- 5b: GIN scan — no match
SELECT id FROM gin_ngram_test
WHERE to_tsvector('ngram', content) @@ plainto_tsquery('ngram', '区块链')
ORDER BY id;

-- 5c: GIN scan with english_stem tokenizer
DROP TABLE IF EXISTS gin_stem_test;
CREATE TABLE gin_stem_test (
    id SERIAL PRIMARY KEY,
    content TEXT
);

INSERT INTO gin_stem_test (content) VALUES
    ('The running dogs are playing'),
    ('Happy cats sitting quietly'),
    ('Dogs and cats are friends'),
    ('Running is good exercise');

CREATE INDEX idx_gin_stem ON gin_stem_test USING gin (to_tsvector('english_stem', content));

-- Stemmed search: 'run' matches 'running'
SELECT id FROM gin_stem_test
WHERE to_tsvector('english_stem', content) @@ plainto_tsquery('english_stem', 'run')
ORDER BY id;

-- Stemmed search: 'dog' matches 'dogs'
SELECT id FROM gin_stem_test
WHERE to_tsvector('english_stem', content) @@ plainto_tsquery('english_stem', 'dog')
ORDER BY id;

-- ============================================================
-- Section 6: CREATE/DROP TEXT SEARCH CONFIGURATION DDL
-- ============================================================

-- 6a: Create a zhparser-based config
CREATE TEXT SEARCH CONFIGURATION test_zhcfg (PARSER = zhparser);

-- 6b: Use the user-defined config for tokenization
SELECT to_tsvector('test_zhcfg', '分布式数据库系统');

-- 6c: ALTER with ADD MAPPING is accepted (no-op)
ALTER TEXT SEARCH CONFIGURATION test_zhcfg ADD MAPPING FOR word WITH simple;

-- 6d: Drop the config
DROP TEXT SEARCH CONFIGURATION test_zhcfg;

-- 6e: DROP IF EXISTS (no error when already dropped)
DROP TEXT SEARCH CONFIGURATION IF EXISTS test_zhcfg;

-- 6f: Create with USING syntax
CREATE TEXT SEARCH CONFIGURATION test_zhcfg2 USING zhparser;
SELECT to_tsvector('test_zhcfg2', '自然语言处理');
DROP TEXT SEARCH CONFIGURATION test_zhcfg2;

-- 6g: Unsupported parser should fail with 0A000
CREATE TEXT SEARCH CONFIGURATION test_bad (PARSER = default);

-- Cleanup
DROP TABLE IF EXISTS gin_ngram_test;
DROP TABLE IF EXISTS gin_stem_test;

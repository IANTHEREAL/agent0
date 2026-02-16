-- Test GIN index support for Chinese full-text search

-- Cleanup
DROP TABLE IF EXISTS articles;

-- Create test table
CREATE TABLE articles (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    content TEXT
);

-- Insert Chinese test data
INSERT INTO articles (title, content) VALUES
    ('数据库技术', '分布式数据库是现代互联网架构的核心组件'),
    ('搜索引擎原理', '全文搜索引擎使用倒排索引来加速查询'),
    ('机器学习入门', '深度学习是机器学习的一个重要分支');

-- Test 1: Create GIN index with Chinese tokenizer (expression index)
CREATE INDEX idx_content_cn ON articles
  USING gin (to_tsvector('chinese', content));

-- Test 2: Query with Chinese tokenizer (exact match)
SELECT id, title FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '数据库')
ORDER BY id;
-- Expected: id=1

-- Test 3: Query with multiple terms
SELECT id, title FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '搜索 索引')
ORDER BY id;
-- Expected: id=2

-- Test 4: Query with ts_rank
SELECT id, title,
       ts_rank(to_tsvector('chinese', content), plainto_tsquery('chinese', '机器学习')) as rank
FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '机器学习')
ORDER BY rank DESC, id;
-- Expected: id=3, rank > 0

-- Test 5: Insert more data and verify index maintenance
INSERT INTO articles (title, content) VALUES
    ('自然语言处理', '自然语言处理是人工智能的重要应用领域');

SELECT id, title FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '自然语言')
ORDER BY id;
-- Expected: id=4

-- Test 6: English tokenizer (for comparison)
CREATE INDEX idx_content_en ON articles
  USING gin (to_tsvector('english', title));

INSERT INTO articles (title, content) VALUES
    ('Database Systems', 'Distributed databases are essential for scalability');

SELECT id, title FROM articles
WHERE to_tsvector('english', title) @@ plainto_tsquery('english', 'database')
ORDER BY id;
-- Expected: id=5

-- Test 7: Mixed Chinese-English content
INSERT INTO articles (title, content) VALUES
    ('PostgreSQL教程', 'PostgreSQL是一个功能强大的开源数据库');

SELECT id, title FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '数据库')
ORDER BY id;
-- Expected: id=1, id=6

-- Test 8: Test to_tsvector directly with Chinese
SELECT to_tsvector('chinese', '我爱中国');

-- Test 9: Test plainto_tsquery with Chinese
SELECT plainto_tsquery('chinese', '人工智能 机器学习');

-- Test 10: Test unknown config error
SELECT to_tsvector('klingon', 'test');
-- Expected: ERROR

-- Test 11: Single-argument to_tsvector (uses default tokenizer)
SELECT to_tsvector('This is a test');

-- Test 12: Verify index usage with EXPLAIN
EXPLAIN SELECT id FROM articles
WHERE to_tsvector('chinese', content) @@ plainto_tsquery('chinese', '数据库');

-- Cleanup
DROP TABLE articles;

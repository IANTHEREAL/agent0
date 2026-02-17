-- Test GIN index support for FTS (tsvector @@ tsquery)

-- Cleanup
DROP TABLE IF EXISTS fts_articles;

-- Create table with tsvector column
CREATE TABLE fts_articles (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    content TEXT,
    search_vector TSVECTOR
);

-- Create GIN index on tsvector column
CREATE INDEX idx_fts_search ON fts_articles USING gin (search_vector);

-- Insert test data with manually constructed tsvectors
INSERT INTO fts_articles (title, content, search_vector) VALUES
    ('PostgreSQL Tutorial', 'Learn about PostgreSQL database', to_tsvector('postgresql tutorial learn database')),
    ('TiKV Architecture', 'Distributed key-value storage', to_tsvector('tikv architecture distributed storage')),
    ('SQL Optimization', 'Query optimization techniques', to_tsvector('sql optimization query techniques')),
    ('Database Indexing', 'B-tree and GIN indexes', to_tsvector('database indexing btree gin indexes')),
    ('Rust Programming', 'Systems programming with Rust', to_tsvector('rust programming systems'));

-- Test 1: Basic FTS match with @@
SELECT id, title FROM fts_articles 
WHERE search_vector @@ plainto_tsquery('postgresql') 
ORDER BY id;

-- Test 2: Multiple term search (AND)
SELECT id, title FROM fts_articles 
WHERE search_vector @@ plainto_tsquery('database indexing') 
ORDER BY id;

-- Test 3: Search with ts_rank ordering
SELECT id, title, ts_rank(search_vector, plainto_tsquery('database')) as rank
FROM fts_articles
WHERE search_vector @@ plainto_tsquery('database')
ORDER BY rank DESC, id;

-- Test 4: No match case
SELECT id, title FROM fts_articles 
WHERE search_vector @@ plainto_tsquery('nonexistent') 
ORDER BY id;

-- Test 5: Verify index is used (via EXPLAIN)
-- GIN index selection is not yet supported in the CBO optimizer path;
-- force legacy planner so EXPLAIN shows the GIN index name.
SET tipg.use_optimizer = off;
EXPLAIN SELECT id FROM fts_articles WHERE search_vector @@ plainto_tsquery('postgresql');
SET tipg.use_optimizer = on;

-- Cleanup
DROP TABLE fts_articles;

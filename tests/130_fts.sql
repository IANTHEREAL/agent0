-- Full-Text Search (FTS) MVP Tests

-- Test to_tsvector with single argument
-- Use explicit config to keep output deterministic regardless of session default_tsc.
SELECT to_tsvector('english', 'The quick brown fox');

-- Test to_tsvector with config argument (config is ignored in MVP)
SELECT to_tsvector('english', 'Hello World');

-- Test plainto_tsquery
SELECT plainto_tsquery('quick fox');

-- Test plainto_tsquery with config
SELECT plainto_tsquery('english', 'hello world');

-- Test to_tsquery (pass-through in MVP)
SELECT to_tsquery('hello & world');

-- Test @@ operator (match)
SELECT to_tsvector('The quick brown fox') @@ plainto_tsquery('quick fox');

-- Test @@ operator (no match)
SELECT to_tsvector('The quick brown fox') @@ plainto_tsquery('lazy dog');

-- Test ts_rank
SELECT ts_rank(to_tsvector('The quick brown fox'), plainto_tsquery('quick fox')) > 0;

-- Test ts_rank with no match
SELECT ts_rank(to_tsvector('hello world'), plainto_tsquery('foo bar'));

-- Test setweight (returns tsvector unchanged in MVP)
SELECT setweight(to_tsvector('hello world'), 'A');

-- Test NULL handling
SELECT to_tsvector(NULL);
SELECT plainto_tsquery(NULL);
SELECT to_tsvector('test') @@ NULL::tsquery;

-- Test with table
CREATE TABLE fts_docs (
    id SERIAL PRIMARY KEY,
    title TEXT,
    body TEXT
);

INSERT INTO fts_docs (title, body) VALUES 
    ('PostgreSQL Guide', 'A comprehensive guide to PostgreSQL database'),
    ('TiKV Overview', 'TiKV is a distributed key-value storage'),
    ('SQL Tutorial', 'Learn SQL queries and database management');

-- Search using @@ operator
SELECT id, title FROM fts_docs 
WHERE to_tsvector(body) @@ plainto_tsquery('database')
ORDER BY id;

-- Search with ts_rank for ordering
SELECT id, title, ts_rank(to_tsvector(body), plainto_tsquery('database')) as rank
FROM fts_docs
WHERE to_tsvector(body) @@ plainto_tsquery('database')
ORDER BY rank DESC, id;

-- Search that returns no results
SELECT id, title FROM fts_docs 
WHERE to_tsvector(body) @@ plainto_tsquery('python')
ORDER BY id;

-- Cleanup
DROP TABLE fts_docs;

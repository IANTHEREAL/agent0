-- RETURNS TABLE(...) support for SQL-language functions

DROP FUNCTION IF EXISTS search_items(text);
DROP FUNCTION IF EXISTS get_stats();
DROP FUNCTION IF EXISTS vec_search(vector(3), int);
DROP TABLE IF EXISTS items;

CREATE TABLE items (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    category TEXT,
    price DOUBLE PRECISION,
    embedding VECTOR(3),
    tsv TSVECTOR
);

INSERT INTO items (id, name, category, price, embedding, tsv) VALUES
(1, 'PostgreSQL Book',   'books',   29.99, '[1.0, 0.0, 0.0]',
   to_tsvector('english', 'PostgreSQL database administration guide')),
(2, 'TiKV Deep Dive',    'books',   39.99, '[0.9, 0.1, 0.0]',
   to_tsvector('english', 'TiKV distributed key-value storage internals')),
(3, 'Rust Cookbook',      'books',   24.99, '[0.0, 1.0, 0.0]',
   to_tsvector('english', 'Rust programming language recipes and patterns')),
(4, 'Wireless Keyboard', 'electronics', 59.99, '[0.0, 0.0, 1.0]',
   to_tsvector('english', 'Mechanical wireless keyboard for programmers'));

-- Test 1: basic RETURNS TABLE with FTS
CREATE OR REPLACE FUNCTION search_items(query TEXT)
RETURNS TABLE(item_name TEXT, snippet TEXT, rank DOUBLE PRECISION)
LANGUAGE sql AS $$
    SELECT
        i.name,
        LEFT(i.name, 50),
        ts_rank(i.tsv, plainto_tsquery('english', query))
    FROM items i
    WHERE i.tsv @@ plainto_tsquery('english', query)
    ORDER BY ts_rank(i.tsv, plainto_tsquery('english', query)) DESC
    LIMIT 10;
$$;

SELECT item_name, rank > 0 AS has_rank FROM search_items('database') ORDER BY item_name;

SELECT item_name FROM search_items('programming') ORDER BY item_name;

SELECT item_name FROM search_items('nonexistent_xyz');

-- Test 2: RETURNS TABLE with no parameters
CREATE OR REPLACE FUNCTION get_stats()
RETURNS TABLE(category TEXT, item_count BIGINT, avg_price DOUBLE PRECISION)
LANGUAGE sql AS $$
    SELECT category, count(*) AS item_count, avg(price) AS avg_price
    FROM items
    GROUP BY category
    ORDER BY category;
$$;

SELECT category, item_count FROM get_stats() ORDER BY category;

-- Test 3: RETURNS TABLE with vector operations
CREATE OR REPLACE FUNCTION vec_search(query_vec vector(3), max_results int)
RETURNS TABLE(item_name TEXT, distance DOUBLE PRECISION)
LANGUAGE sql AS $$
    SELECT i.name, i.embedding <-> query_vec AS dist
    FROM items i
    ORDER BY dist
    LIMIT max_results;
$$;

SELECT item_name, distance < 1.0 AS is_close FROM vec_search('[1,0,0]'::vector(3), 2) ORDER BY item_name;

-- Cleanup
DROP FUNCTION IF EXISTS search_items(text);
DROP FUNCTION IF EXISTS get_stats();
DROP FUNCTION IF EXISTS vec_search(vector(3), int);
DROP TABLE IF EXISTS items;

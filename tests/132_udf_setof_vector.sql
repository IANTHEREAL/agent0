-- Test: SETOF user-defined functions + pgvector distance operators
-- Covers Sprint 6.1 (vector operators) + Sprint 6.2/6.3 (FROM clause SRF dispatch)

-- Setup
DROP TABLE IF EXISTS vec_docs;

CREATE TABLE vec_docs (
    id BIGINT PRIMARY KEY,
    title TEXT NOT NULL,
    embedding vector(3)
);

INSERT INTO vec_docs VALUES (1, 'database', '[1,0,0]');
INSERT INTO vec_docs VALUES (2, 'embeddings', '[0,1,0]');
INSERT INTO vec_docs VALUES (3, 'mixed', '[0.7,0.7,0]');

-- Test 1: pgvector distance operators
SELECT id, title, embedding <-> '[1,0,0]'::vector(3) AS l2_dist
FROM vec_docs ORDER BY id;

SELECT id, title, embedding <=> '[1,0,0]'::vector(3) AS cos_dist
FROM vec_docs ORDER BY id;

SELECT id, title, embedding <#> '[1,0,0]'::vector(3) AS ip
FROM vec_docs ORDER BY id;

-- Test 2: ORDER BY with vector distance operator
SELECT id, title FROM vec_docs ORDER BY embedding <-> '[1,0,0]'::vector(3) LIMIT 2;

-- Test 3: Create a SETOF function
CREATE FUNCTION match_vec_docs(query_emb vector(3), match_count int)
RETURNS SETOF vec_docs LANGUAGE sql AS $$
  SELECT * FROM vec_docs
  ORDER BY embedding <-> query_emb
  LIMIT match_count;
$$;

-- Test 4: Call SETOF function from FROM clause
SELECT id, title FROM match_vec_docs('[1,0,0]'::vector(3), 2) ORDER BY id;

-- Test 5: Simple SETOF function without vector ops
CREATE FUNCTION get_all_docs()
RETURNS SETOF vec_docs LANGUAGE sql AS $$
  SELECT * FROM vec_docs ORDER BY id;
$$;

SELECT id, title FROM get_all_docs();

-- Test 6: SETOF with WHERE in function body
CREATE FUNCTION docs_by_title(pattern text)
RETURNS SETOF vec_docs LANGUAGE sql AS $$
  SELECT * FROM vec_docs WHERE title LIKE pattern ORDER BY id;
$$;

SELECT id, title FROM docs_by_title('%base%');

-- Cleanup
DROP FUNCTION IF EXISTS match_vec_docs;
DROP FUNCTION IF EXISTS get_all_docs;
DROP FUNCTION IF EXISTS docs_by_title;
DROP TABLE IF EXISTS vec_docs;

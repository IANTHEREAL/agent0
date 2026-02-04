-- Setup
DROP FUNCTION IF EXISTS match_documents;
DROP FUNCTION IF EXISTS match_docs_cosine;
DROP FUNCTION IF EXISTS get_all_docs;
DROP FUNCTION IF EXISTS docs_by_title;
DROP TABLE IF EXISTS documents;

CREATE TABLE documents (
    id BIGINT PRIMARY KEY,
    title TEXT NOT NULL,
    content TEXT,
    embedding vector(3)
);

INSERT INTO documents (id, title, content, embedding) VALUES
(1, 'PostgreSQL overview', 'PostgreSQL is an advanced open source database.', '[1.0, 0.0, 0.0]'),
(2, 'Vector search', 'Vector similarity search using embeddings.', '[0.9, 0.1, 0.0]'),
(3, 'AI embeddings', 'Embeddings map text into vector space.', '[0.0, 1.0, 0.0]'),
(4, 'Cooking pasta', 'How to cook pasta properly.', '[0.0, 0.0, 1.0]');

-- Test 1: distance operators in SELECT
SELECT id, title, embedding <-> '[1,0,0]'::vector(3) AS l2_dist FROM documents ORDER BY id;

SELECT id, title, embedding <=> '[1,0,0]'::vector(3) AS cos_dist FROM documents ORDER BY id;

SELECT id, title, embedding <#> '[1,0,0]'::vector(3) AS neg_ip FROM documents ORDER BY id;

-- Test 2: ORDER BY with distance operator + comparison
SELECT id, title FROM documents WHERE embedding <-> '[1,0,0]'::vector(3) < 1.0 ORDER BY embedding <-> '[1,0,0]'::vector(3) LIMIT 2;

-- Test 3: <#> with comparison in WHERE (the Supabase RAG pattern)
SELECT id, title FROM documents WHERE embedding <#> '[1,0,0]'::vector(3) < -0.2 ORDER BY embedding <#> '[1,0,0]'::vector(3) ASC;

-- Test 4: SETOF function with <#> operator (negative inner product)
CREATE OR REPLACE FUNCTION match_documents(
  query_embedding vector(3),
  match_threshold float,
  match_count int
)
RETURNS SETOF documents
LANGUAGE sql
AS $$
  SELECT *
  FROM documents
  WHERE documents.embedding <#> query_embedding < -match_threshold
  ORDER BY documents.embedding <#> query_embedding ASC
  LIMIT least(match_count, 200);
$$;

-- "database" query: expect PostgreSQL overview + Vector search
SELECT id, title FROM match_documents('[1.0, 0.0, 0.0]'::vector(3), 0.2, 10) ORDER BY id;

-- "AI" query: expect AI embeddings
SELECT id, title FROM match_documents('[0.0, 1.0, 0.0]'::vector(3), 0.2, 10) ORDER BY id;

-- "cooking" query: expect Cooking pasta
SELECT id, title FROM match_documents('[0.0, 0.0, 1.0]'::vector(3), 0.2, 10) ORDER BY id;

-- Test 5: SETOF function with <=> operator (cosine distance)
CREATE OR REPLACE FUNCTION match_docs_cosine(
  query_embedding vector(3),
  match_threshold float,
  match_count int
)
RETURNS SETOF documents
LANGUAGE sql
AS $$
  SELECT *
  FROM documents
  WHERE (documents.embedding <=> query_embedding) < match_threshold
  ORDER BY documents.embedding <=> query_embedding ASC
  LIMIT least(match_count, 200);
$$;

SELECT id, title FROM match_docs_cosine('[1.0, 0.0, 0.0]'::vector(3), 0.3, 10) ORDER BY id;

-- Test 6: simple SETOF (no vector ops)
CREATE FUNCTION get_all_docs()
RETURNS SETOF documents LANGUAGE sql AS $$
  SELECT * FROM documents ORDER BY id;
$$;

SELECT id, title FROM get_all_docs();

-- Test 7: SETOF with WHERE param
CREATE FUNCTION docs_by_title(pattern text)
RETURNS SETOF documents LANGUAGE sql AS $$
  SELECT * FROM documents WHERE title LIKE pattern ORDER BY id;
$$;

SELECT id, title FROM docs_by_title('%pasta%');

-- Cleanup
DROP FUNCTION IF EXISTS match_documents;
DROP FUNCTION IF EXISTS match_docs_cosine;
DROP FUNCTION IF EXISTS get_all_docs;
DROP FUNCTION IF EXISTS docs_by_title;
DROP TABLE IF EXISTS documents;

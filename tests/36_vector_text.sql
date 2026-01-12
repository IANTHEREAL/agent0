-- Test vector functions with TEXT input
-- Tests the extract_vector text parsing implementation

-- Create table with text column for embeddings (how ORMs store vectors)
CREATE TABLE IF NOT EXISTS test_embeddings (
    id SERIAL PRIMARY KEY,
    name TEXT,
    embedding TEXT
);

-- Insert test data
INSERT INTO test_embeddings (name, embedding) VALUES
    ('doc1', '[1.0, 2.0, 3.0]'),
    ('doc2', '[4.0, 5.0, 6.0]'),
    ('doc3', '[1.0, 0.0, 0.0]');

-- Test l2_distance with text column and text literal
SELECT name, l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance
FROM test_embeddings
ORDER BY distance ASC;

-- Test cosine_distance
SELECT name, cosine_distance(embedding, '[1.0, 1.0, 1.0]') as similarity
FROM test_embeddings
ORDER BY similarity ASC;

-- Test inner_product
SELECT name, inner_product(embedding, '[1.0, 1.0, 1.0]') as product
FROM test_embeddings
ORDER BY product DESC;

-- Test vector_dims on text column
SELECT name, vector_dims(embedding) as dims
FROM test_embeddings;

-- Test vector_norm on text column
SELECT name, vector_norm(embedding) as norm
FROM test_embeddings;

-- Cleanup
DROP TABLE test_embeddings;

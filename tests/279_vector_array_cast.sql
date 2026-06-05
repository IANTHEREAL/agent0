-- pgvector-compatible array-to-vector casts.
DROP TABLE IF EXISTS vector_array_cast_test;

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE vector_array_cast_test (
    id SERIAL PRIMARY KEY,
    content TEXT NOT NULL,
    embedding vector(3)
);

INSERT INTO vector_array_cast_test (content, embedding) VALUES
    ('string literal', '[0.1, 0.2, 0.3]'),
    ('array cast', ARRAY[0.2, 0.1, 0.4]::vector),
    ('array cast typmod', ARRAY[0.3, 0.2, 0.1]::vector(3));

SELECT content, vector_dims(embedding)
FROM vector_array_cast_test
ORDER BY id;

DROP TABLE vector_array_cast_test;

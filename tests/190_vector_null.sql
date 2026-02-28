DROP TABLE IF EXISTS vec_null_test;
CREATE TABLE vec_null_test (id SERIAL PRIMARY KEY, content TEXT, embedding vector(3));
INSERT INTO vec_null_test (content, embedding) VALUES
    ('has vec', '[0.1, 0.2, 0.3]'),
    ('null vec', NULL),
    ('another', '[0.4, 0.5, 0.6]');

SELECT content, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
FROM vec_null_test ORDER BY id;

SELECT content, embedding <=> '[0.1, 0.2, 0.3]' AS dist
FROM vec_null_test ORDER BY dist;

SELECT content, embedding <=> '[0.1, 0.2, 0.3]' AS dist
FROM vec_null_test ORDER BY dist NULLS FIRST;

SELECT l2_distance(NULL::vector, '[0.1, 0.2, 0.3]');

SELECT vector_dims(NULL::vector);

SELECT content, embedding <=> '[0.1, 0.2, 0.3]' AS dist
FROM vec_null_test WHERE embedding IS NOT NULL ORDER BY dist;

DROP TABLE vec_null_test;

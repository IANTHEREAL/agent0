DROP TABLE IF EXISTS vec_cmp_test;

CREATE TABLE vec_cmp_test (id SERIAL PRIMARY KEY, embedding vector(3));

INSERT INTO vec_cmp_test (embedding) VALUES
    ('[0.1, 0.2, 0.3]'),
    ('[0.4, 0.5, 0.6]'),
    ('[0.1, 0.2, 0.3]'),
    ('[0.7, 0.8, 0.9]');

SELECT id, embedding
FROM vec_cmp_test
WHERE embedding = '[0.1, 0.2, 0.3]'
ORDER BY id;

SELECT id, embedding
FROM vec_cmp_test
WHERE embedding <> '[0.1, 0.2, 0.3]'
ORDER BY id;

SELECT id, embedding
FROM vec_cmp_test
ORDER BY embedding, id;

SELECT DISTINCT embedding
FROM vec_cmp_test
ORDER BY embedding;

SELECT id, embedding
FROM vec_cmp_test
WHERE embedding < '[0.4, 0.5, 0.6]'
ORDER BY id;

SELECT id, embedding
FROM vec_cmp_test
WHERE embedding >= '[0.4, 0.5, 0.6]'
ORDER BY id;

DROP TABLE vec_cmp_test;

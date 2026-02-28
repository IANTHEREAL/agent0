DROP TABLE IF EXISTS vec_group_test;

CREATE TABLE vec_group_test (
    id SERIAL PRIMARY KEY,
    content TEXT,
    embedding vector(3)
);

INSERT INTO vec_group_test (content, embedding) VALUES
    ('doc1', '[0.1, 0.2, 0.3]'),
    ('doc2', '[0.4, 0.5, 0.6]'),
    ('doc1', '[0.1, 0.2, 0.3]');

SELECT content, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
FROM vec_group_test
GROUP BY id, content, embedding
ORDER BY dist; -- db9 divergence: cosine_distance precision differs from pgvector

\echo -- db9 divergence: cosine_distance precision differs from pgvector (PG: 0.02536811254398652, db9: 0.025368153802923787)

SELECT content, dist
FROM (
    SELECT content, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
    FROM vec_group_test
    GROUP BY id, content, embedding
) AS g
ORDER BY content, dist; -- db9 divergence: cosine_distance precision differs from pgvector

\echo -- db9 divergence: cosine_distance precision differs from pgvector (PG: 0.02536811254398652, db9: 0.025368153802923787)

SELECT content, length(content) AS len
FROM vec_group_test
GROUP BY id, content, embedding
ORDER BY len, content;

SELECT content, COUNT(*) AS cnt, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
FROM vec_group_test
GROUP BY content, embedding
ORDER BY dist; -- db9 divergence: cosine_distance precision differs from pgvector

\echo -- db9 divergence: cosine_distance precision differs from pgvector (PG: 0.02536811254398652, db9: 0.025368153802923787)

DROP TABLE vec_group_test;

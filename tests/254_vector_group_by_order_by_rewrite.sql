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
ORDER BY dist;

SELECT content, dist
FROM (
    SELECT content, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
    FROM vec_group_test
    GROUP BY id, content, embedding
) AS g
ORDER BY content, dist;

SELECT content, length(content) AS len
FROM vec_group_test
GROUP BY id, content, embedding
ORDER BY len, content;

SELECT content, COUNT(*) AS cnt, cosine_distance(embedding, '[0.1, 0.2, 0.3]') AS dist
FROM vec_group_test
GROUP BY content, embedding
ORDER BY dist;

DROP TABLE vec_group_test;

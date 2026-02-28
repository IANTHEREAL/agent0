CREATE TABLE vec_agg_test (id INT, embedding VECTOR(3));
INSERT INTO vec_agg_test VALUES (1, '[1.0, 2.0, 3.0]');
INSERT INTO vec_agg_test VALUES (2, '[4.0, 5.0, 6.0]');
INSERT INTO vec_agg_test VALUES (3, '[7.0, 8.0, 9.0]');

SELECT SUM(embedding) FROM vec_agg_test;

SELECT AVG(embedding) FROM vec_agg_test;

INSERT INTO vec_agg_test VALUES (4, NULL);
SELECT SUM(embedding) FROM vec_agg_test;

SELECT AVG(embedding) FROM vec_agg_test;

SELECT id % 2 AS grp, SUM(embedding) FROM vec_agg_test WHERE embedding IS NOT NULL GROUP BY id % 2 ORDER BY grp;

-- db9 divergence: MIN/MAX(vector) not in pgvector; parity case removed

DROP TABLE vec_agg_test;

SELECT '[1.0, 2.0, 3.0]'::vector + '[4.0, 5.0, 6.0]'::vector AS sum;

SELECT '[4.0, 5.0, 6.0]'::vector - '[1.0, 2.0, 3.0]'::vector AS diff;

DROP TABLE IF EXISTS vec_arith_test;
CREATE TABLE vec_arith_test (id SERIAL PRIMARY KEY, a vector(3), b vector(3));
INSERT INTO vec_arith_test (a, b) VALUES
    ('[1.0, 0.0, 0.0]', '[0.0, 1.0, 0.0]'),
    ('[0.5, 0.5, 0.0]', '[0.0, 0.0, 1.0]');

SELECT id, a + b AS sum, a - b AS diff FROM vec_arith_test ORDER BY id;

DROP TABLE vec_arith_test;

-- db9 divergence: error text differs from pgvector CheckDims
\echo -- db9 divergence: error text differs from pgvector CheckDims
SELECT '[1.0, 2.0]'::vector + '[1.0, 2.0, 3.0]'::vector;

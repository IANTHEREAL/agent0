-- L2 normalize function test
-- Basic normalization
SELECT l2_normalize('[3.0, 4.0]'::vector);

-- Unit vector stays the same
SELECT l2_normalize('[1.0, 0.0, 0.0]'::vector);

-- Zero vector
SELECT l2_normalize('[0.0, 0.0]'::vector);

-- NULL
SELECT l2_normalize(NULL::vector);

-- Verify normalized vector has unit norm
\echo -- db9 divergence: vector_norm precision differs from pgvector (PG: 1.000000023841858, db9: 1)
SELECT vector_norm(l2_normalize('[3.0, 4.0]'::vector)); -- db9 divergence: vector_norm precision differs from pgvector

-- Test with different dimensions
SELECT l2_normalize('[1.0, 1.0, 1.0, 1.0]'::vector);

-- Test with negative values
SELECT l2_normalize('[-3.0, -4.0]'::vector);

-- Test with mixed positive/negative
\echo -- db9 divergence: l2_normalize precision differs from pgvector (PG: [0.4082483,-0.4082483,0.8164966], db9: [0.4082482904638631,-0.4082482904638631,0.8164965809277261])
SELECT l2_normalize('[1.0, -1.0, 2.0]'::vector); -- db9 divergence: l2_normalize precision differs from pgvector

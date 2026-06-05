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
SELECT vector_norm(l2_normalize('[3.0, 4.0]'::vector));

-- Test with different dimensions
SELECT l2_normalize('[1.0, 1.0, 1.0, 1.0]'::vector);

-- Test with negative values
SELECT l2_normalize('[-3.0, -4.0]'::vector);

-- Test with mixed positive/negative
SELECT l2_normalize('[1.0, -1.0, 2.0]'::vector);

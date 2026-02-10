-- Test that invalid casts produce errors (not silent defaults)
-- Fixes: issue #406

-- Valid casts should still work
SELECT '123'::INT AS valid_int;
SELECT '-456'::INT AS valid_neg_int;
SELECT '789'::BIGINT AS valid_bigint;
SELECT '3.14'::FLOAT8 AS valid_float;
SELECT 'true'::BOOLEAN AS valid_bool_true;
SELECT 'false'::BOOLEAN AS valid_bool_false;
SELECT 'f'::BOOLEAN AS valid_bool_f;
SELECT 'yes'::BOOLEAN AS valid_bool_yes;

-- Invalid text-to-int must error (previously returned 0)
SELECT 'abc'::INT;

-- Invalid empty string to int must error (previously returned 0)
SELECT ''::INT;

-- Invalid text-to-bigint must error
SELECT 'xyz'::BIGINT;

-- Invalid text-to-float must error (previously returned 0.0)
SELECT 'not_a_float'::FLOAT8;

-- Invalid text-to-boolean must error (previously returned false)
SELECT 'nope'::BOOLEAN;

-- Invalid text-to-boolean: another bad value
SELECT 'maybe'::BOOLEAN;

-- Narrowing cast overflow: bigint out of i32 range must error (previously wrapped)
SELECT 2147483648::BIGINT::INT4;

-- Narrowing cast overflow: large float to int must error (previously saturated)
SELECT 1e20::FLOAT8::INT4;

-- NaN to int must error (previously returned 0)
SELECT 'NaN'::FLOAT8::INT4;

-- Float to bigint overflow must error
SELECT 1e19::FLOAT8::BIGINT;

-- NaN to bigint must error
SELECT 'NaN'::FLOAT8::BIGINT;

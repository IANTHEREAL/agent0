-- Test ARRAY type protocol - verify arrays are returned with proper OIDs

-- Test integer arrays
SELECT ARRAY[1, 2, 3] AS int_array;

SELECT ARRAY[1::BIGINT, 2::BIGINT, 3::BIGINT] AS bigint_array;

-- Test text arrays  
SELECT ARRAY['a', 'b', 'c'] AS text_array;

-- Test boolean arrays
SELECT ARRAY[true, false, true] AS bool_array;

-- Test float arrays
SELECT ARRAY[1.5, 2.5, 3.5] AS float_array;

-- Test nested operations with arrays
SELECT ARRAY[1, 2] || ARRAY[3, 4] AS concat_array;

-- Test array in table
DROP TABLE IF EXISTS test_array_col;
CREATE TABLE test_array_col (
    id SERIAL PRIMARY KEY,
    tags TEXT[]
);

INSERT INTO test_array_col (tags) VALUES (ARRAY['rust', 'tikv']);
INSERT INTO test_array_col (tags) VALUES (ARRAY['postgres', 'sql']);

SELECT id, tags FROM test_array_col ORDER BY id;

DROP TABLE test_array_col;

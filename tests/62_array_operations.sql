-- Array Operations Test
-- Tests array literals, functions, and operators

-- Array literals
SELECT ARRAY[1, 2, 3] AS int_array;
SELECT ARRAY['a', 'b', 'c'] AS text_array;
SELECT ARRAY[1.5, 2.5, 3.5] AS float_array;

-- Array length
SELECT ARRAY_LENGTH(ARRAY[1, 2, 3, 4, 5], 1) AS arr_len;
SELECT CARDINALITY(ARRAY[1, 2, 3]) AS cardinality;

-- Array access (1-indexed)
SELECT (ARRAY[10, 20, 30])[1] AS first_elem;
SELECT (ARRAY[10, 20, 30])[2] AS second_elem;
SELECT (ARRAY[10, 20, 30])[3] AS third_elem;

-- Array concatenation
SELECT ARRAY[1, 2] || ARRAY[3, 4] AS concat_arrays;
SELECT ARRAY[1, 2] || 3 AS append_elem;
SELECT 0 || ARRAY[1, 2] AS prepend_elem;

-- Array contains
SELECT ARRAY[1, 2, 3] @> ARRAY[2] AS contains_2;
SELECT ARRAY[1, 2, 3] @> ARRAY[4] AS contains_4;

-- Array overlap
SELECT ARRAY[1, 2, 3] && ARRAY[2, 4] AS has_overlap;
SELECT ARRAY[1, 2, 3] && ARRAY[4, 5] AS no_overlap;

-- ANY/ALL with arrays
SELECT 2 = ANY(ARRAY[1, 2, 3]) AS two_in_array;
SELECT 5 = ANY(ARRAY[1, 2, 3]) AS five_in_array;
SELECT 2 = ALL(ARRAY[2, 2, 2]) AS all_twos;
SELECT 2 = ALL(ARRAY[1, 2, 3]) AS not_all_twos;

-- Array to string
SELECT ARRAY_TO_STRING(ARRAY[1, 2, 3], ',') AS comma_separated;
SELECT ARRAY_TO_STRING(ARRAY['a', 'b', 'c'], '-') AS dash_separated;

-- String to array
SELECT STRING_TO_ARRAY('1,2,3', ',') AS from_string;
SELECT STRING_TO_ARRAY('a-b-c', '-') AS from_dashes;

-- UNNEST
SELECT UNNEST(ARRAY[1, 2, 3]) AS unnested;

-- Array in table
DROP TABLE IF EXISTS arr_test;
CREATE TABLE arr_test (id INT PRIMARY KEY, tags TEXT[]);
INSERT INTO arr_test VALUES (1, ARRAY['red', 'green']);
INSERT INTO arr_test VALUES (2, ARRAY['blue', 'yellow']);
INSERT INTO arr_test VALUES (3, ARRAY['red', 'blue']);

SELECT id, tags FROM arr_test WHERE 'red' = ANY(tags) ORDER BY id;
SELECT id, tags FROM arr_test WHERE tags @> ARRAY['blue'] ORDER BY id;

DROP TABLE arr_test;

SELECT 'Array operations tests completed' AS result;

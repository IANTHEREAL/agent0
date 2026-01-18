DROP TABLE IF EXISTS comp_test CASCADE;

CREATE TABLE comp_test (
    id INT PRIMARY KEY,
    int_val INT,
    text_val TEXT,
    bool_val BOOLEAN
);

INSERT INTO comp_test VALUES
    (1, 10, 'apple', true),
    (2, 20, 'banana', false),
    (3, NULL, 'cherry', NULL),
    (4, 30, NULL, true),
    (5, 10, 'apple', false);

SELECT id FROM comp_test WHERE int_val = 10 ORDER BY id;
SELECT id FROM comp_test WHERE int_val <> 20 ORDER BY id;
SELECT id FROM comp_test WHERE int_val != 20 ORDER BY id;
SELECT id FROM comp_test WHERE int_val < 20 ORDER BY id;
SELECT id FROM comp_test WHERE int_val <= 20 ORDER BY id;
SELECT id FROM comp_test WHERE int_val > 10 ORDER BY id;
SELECT id FROM comp_test WHERE int_val >= 20 ORDER BY id;

SELECT id FROM comp_test WHERE text_val = 'apple' ORDER BY id;
SELECT id FROM comp_test WHERE text_val < 'banana' ORDER BY id;
SELECT id FROM comp_test WHERE text_val >= 'cherry' ORDER BY id;

SELECT id FROM comp_test WHERE bool_val = true ORDER BY id;
SELECT id FROM comp_test WHERE bool_val = false ORDER BY id;
SELECT id FROM comp_test WHERE bool_val IS TRUE ORDER BY id;
SELECT id FROM comp_test WHERE bool_val IS FALSE ORDER BY id;
SELECT id FROM comp_test WHERE bool_val IS NOT TRUE ORDER BY id;

SELECT id FROM comp_test WHERE int_val IS NULL ORDER BY id;
SELECT id FROM comp_test WHERE int_val IS NOT NULL ORDER BY id;
SELECT id FROM comp_test WHERE text_val IS NULL ORDER BY id;
SELECT id FROM comp_test WHERE bool_val IS UNKNOWN ORDER BY id;

SELECT id FROM comp_test WHERE int_val BETWEEN 10 AND 20 ORDER BY id;
SELECT id FROM comp_test WHERE int_val NOT BETWEEN 10 AND 20 ORDER BY id;
SELECT id FROM comp_test WHERE text_val BETWEEN 'a' AND 'c' ORDER BY id;

SELECT id FROM comp_test WHERE int_val IN (10, 30) ORDER BY id;
SELECT id FROM comp_test WHERE int_val NOT IN (10, 30) ORDER BY id;
SELECT id FROM comp_test WHERE text_val IN ('apple', 'cherry') ORDER BY id;

SELECT id FROM comp_test WHERE text_val LIKE 'a%' ORDER BY id;
SELECT id FROM comp_test WHERE text_val LIKE '%an%' ORDER BY id;
SELECT id FROM comp_test WHERE text_val NOT LIKE 'a%' ORDER BY id;
SELECT id FROM comp_test WHERE text_val ILIKE 'A%' ORDER BY id;

SELECT id FROM comp_test WHERE text_val SIMILAR TO '(apple|banana)' ORDER BY id;

SELECT (10, 'a') = (10, 'a') AS row_eq;
SELECT (10, 'a') <> (10, 'b') AS row_neq;
SELECT (1, 2) < (1, 3) AS row_lt;
SELECT (1, 2) <= (1, 2) AS row_le;

SELECT NULLIF(10, 10) IS NULL AS nullif_null;
SELECT NULLIF(10, 20) AS nullif_value;
SELECT COALESCE(NULL, NULL, 'default') AS coalesce_default;
SELECT COALESCE(1, 2, 3) AS coalesce_first;

DROP TABLE comp_test;

SELECT 'Comparison operators tests completed' AS result;

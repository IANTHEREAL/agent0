DROP TABLE IF EXISTS null_test CASCADE;

CREATE TABLE null_test (
    id INT PRIMARY KEY,
    name TEXT,
    value INT
);

INSERT INTO null_test VALUES (1, 'Alice', 100);
INSERT INTO null_test VALUES (2, 'Bob', NULL);
INSERT INTO null_test VALUES (3, NULL, 200);
INSERT INTO null_test VALUES (4, 'Diana', 150);
INSERT INTO null_test VALUES (5, NULL, NULL);

SELECT 'NULL = NULL' AS test, (NULL = NULL) AS result;
SELECT 'NULL <> NULL' AS test, (NULL <> NULL) AS result;
SELECT '5 = NULL' AS test, (5 = NULL) AS result;

SELECT id, name FROM null_test WHERE name IS NULL ORDER BY id;
SELECT id, name FROM null_test WHERE name IS NOT NULL ORDER BY id;
SELECT COUNT(*) AS null_count FROM null_test WHERE value IS NULL;

SELECT id, COALESCE(name, 'Unknown') AS name_or_default FROM null_test ORDER BY id;
SELECT id, COALESCE(value, 0) AS value_or_zero FROM null_test ORDER BY id;
SELECT COALESCE(NULL, NULL, 'third') AS first_non_null;

SELECT NULLIF(1, 1) AS should_be_null;
SELECT NULLIF(1, 2) AS should_be_one;
SELECT id, NULLIF(value, 100) AS nullif_100 FROM null_test ORDER BY id;

SELECT COUNT(*) AS count_all FROM null_test;
SELECT COUNT(name) AS count_name FROM null_test;
SELECT COUNT(value) AS count_value FROM null_test;
SELECT SUM(value) AS sum_value FROM null_test;
SELECT AVG(value) AS avg_value FROM null_test;

SELECT id FROM null_test WHERE value IN (100, 150) ORDER BY id;
SELECT id FROM null_test WHERE value NOT IN (100, 150) ORDER BY id;

SELECT id FROM null_test WHERE value BETWEEN 100 AND 200 ORDER BY id;

SELECT id, 
       CASE WHEN value IS NULL THEN 'no value'
            WHEN value < 150 THEN 'low'
            ELSE 'high'
       END AS category
FROM null_test ORDER BY id;

SELECT id, value FROM null_test ORDER BY value NULLS FIRST;
SELECT id, value FROM null_test ORDER BY value NULLS LAST;

SELECT DISTINCT value FROM null_test ORDER BY value NULLS FIRST;

DROP TABLE null_test;

SELECT 'NULL handling tests completed' AS result;

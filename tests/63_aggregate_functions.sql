-- Aggregate Functions Comprehensive Test

DROP TABLE IF EXISTS agg_test;
CREATE TABLE agg_test (
    id INT PRIMARY KEY,
    category TEXT,
    value INT,
    price NUMERIC(10,2)
);

INSERT INTO agg_test VALUES (1, 'A', 10, 100.50);
INSERT INTO agg_test VALUES (2, 'A', 20, 200.75);
INSERT INTO agg_test VALUES (3, 'B', 30, 150.25);
INSERT INTO agg_test VALUES (4, 'B', 40, 300.00);
INSERT INTO agg_test VALUES (5, 'B', NULL, 250.50);
INSERT INTO agg_test VALUES (6, 'C', 50, NULL);

-- Basic aggregates
SELECT COUNT(*) AS total_rows FROM agg_test;
SELECT COUNT(value) AS non_null_values FROM agg_test;
SELECT COUNT(DISTINCT category) AS distinct_categories FROM agg_test;

SELECT SUM(value) AS sum_value FROM agg_test;
SELECT AVG(value) AS avg_value FROM agg_test;
SELECT MIN(value) AS min_value FROM agg_test;
SELECT MAX(value) AS max_value FROM agg_test;

-- Aggregates with NUMERIC
SELECT SUM(price) AS sum_price FROM agg_test;
SELECT AVG(price) AS avg_price FROM agg_test;

-- GROUP BY
SELECT category, COUNT(*) AS cnt, SUM(value) AS total 
FROM agg_test 
GROUP BY category 
ORDER BY category;

-- GROUP BY with HAVING
SELECT category, SUM(value) AS total 
FROM agg_test 
GROUP BY category 
HAVING SUM(value) > 30 
ORDER BY category;

-- Multiple aggregates
SELECT 
    category,
    COUNT(*) AS cnt,
    SUM(value) AS sum_val,
    AVG(value) AS avg_val,
    MIN(value) AS min_val,
    MAX(value) AS max_val
FROM agg_test 
GROUP BY category 
ORDER BY category;

-- BOOL_AND / BOOL_OR
DROP TABLE IF EXISTS bool_agg_test;
CREATE TABLE bool_agg_test (id INT, flag BOOLEAN);
INSERT INTO bool_agg_test VALUES (1, true), (2, true), (3, false);

SELECT BOOL_AND(flag) AS all_true FROM bool_agg_test;
SELECT BOOL_OR(flag) AS any_true FROM bool_agg_test;

DROP TABLE bool_agg_test;

-- STRING_AGG
SELECT STRING_AGG(category, ',' ORDER BY category) AS categories FROM agg_test;
SELECT STRING_AGG(DISTINCT category, '-' ORDER BY category) AS distinct_cats FROM agg_test;

-- ARRAY_AGG
SELECT ARRAY_AGG(value ORDER BY value) AS values_arr FROM agg_test WHERE value IS NOT NULL;

-- Aggregate with filter (PostgreSQL 9.4+)
SELECT 
    COUNT(*) FILTER (WHERE value > 20) AS gt_20,
    SUM(value) FILTER (WHERE category = 'B') AS sum_b
FROM agg_test;

DROP TABLE agg_test;

SELECT 'Aggregate functions tests completed' AS result;

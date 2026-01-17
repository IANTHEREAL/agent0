-- Auto tests: Aggregation / GROUP BY / HAVING
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS agg_sales;

CREATE TABLE agg_sales (
    dept TEXT,
    amount INT
);

INSERT INTO agg_sales (dept, amount) VALUES
    ('A', 10),
    ('A', 20),
    ('B', 5),
    ('B', 15),
    ('C', 7);

SELECT dept, COUNT(*) AS cnt, SUM(amount) AS total
FROM agg_sales
GROUP BY dept
ORDER BY dept;

SELECT dept, AVG(amount) AS avg_amount
FROM agg_sales
GROUP BY dept
HAVING AVG(amount) > 10
ORDER BY dept;

SELECT MIN(amount) AS min_amount, MAX(amount) AS max_amount FROM agg_sales;

DROP TABLE agg_sales;

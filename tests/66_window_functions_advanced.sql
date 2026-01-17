-- Advanced Window Functions Test

DROP TABLE IF EXISTS sales;
CREATE TABLE sales (
    id INT PRIMARY KEY,
    region TEXT,
    product TEXT,
    amount INT,
    sale_date DATE
);

INSERT INTO sales VALUES (1, 'East', 'A', 100, '2024-01-01');
INSERT INTO sales VALUES (2, 'East', 'B', 150, '2024-01-02');
INSERT INTO sales VALUES (3, 'West', 'A', 200, '2024-01-01');
INSERT INTO sales VALUES (4, 'West', 'B', 120, '2024-01-03');
INSERT INTO sales VALUES (5, 'East', 'A', 180, '2024-01-04');
INSERT INTO sales VALUES (6, 'West', 'A', 90, '2024-01-05');

-- ROW_NUMBER
SELECT id, region, amount, ROW_NUMBER() OVER (ORDER BY amount DESC) AS rank_all
FROM sales ORDER BY rank_all;

-- ROW_NUMBER with PARTITION BY
SELECT id, region, amount, ROW_NUMBER() OVER (PARTITION BY region ORDER BY amount DESC) AS rank_in_region
FROM sales ORDER BY region, rank_in_region;

-- RANK (with ties)
SELECT id, region, amount, RANK() OVER (ORDER BY amount DESC) AS rank_val
FROM sales ORDER BY rank_val, id;

-- DENSE_RANK
SELECT id, region, amount, DENSE_RANK() OVER (ORDER BY amount DESC) AS dense_rank_val
FROM sales ORDER BY dense_rank_val, id;

-- SUM window
SELECT id, region, amount, 
    SUM(amount) OVER (PARTITION BY region ORDER BY id) AS running_total
FROM sales ORDER BY region, id;

-- AVG window
SELECT id, region, amount,
    AVG(amount) OVER (PARTITION BY region) AS region_avg
FROM sales ORDER BY region, id;

-- COUNT window
SELECT id, region, amount,
    COUNT(*) OVER (PARTITION BY region) AS region_count
FROM sales ORDER BY region, id;

-- MIN/MAX window
SELECT id, region, amount,
    MIN(amount) OVER (PARTITION BY region) AS region_min,
    MAX(amount) OVER (PARTITION BY region) AS region_max
FROM sales ORDER BY region, id;

-- LAG
SELECT id, region, amount,
    LAG(amount) OVER (ORDER BY id) AS prev_amount,
    LAG(amount, 2) OVER (ORDER BY id) AS prev2_amount
FROM sales ORDER BY id;

-- LEAD
SELECT id, region, amount,
    LEAD(amount) OVER (ORDER BY id) AS next_amount,
    LEAD(amount, 1, 0) OVER (ORDER BY id) AS next_or_zero
FROM sales ORDER BY id;

-- FIRST_VALUE / LAST_VALUE
SELECT id, region, amount,
    FIRST_VALUE(amount) OVER (PARTITION BY region ORDER BY id) AS first_in_region
FROM sales ORDER BY region, id;

-- Multiple window functions in same query
SELECT id, region, amount,
    ROW_NUMBER() OVER (ORDER BY amount DESC) AS overall_rank,
    ROW_NUMBER() OVER (PARTITION BY region ORDER BY amount DESC) AS region_rank,
    SUM(amount) OVER (PARTITION BY region) AS region_total
FROM sales ORDER BY region, id;

-- Window with ROWS BETWEEN (if supported)
SELECT id, amount,
    SUM(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS moving_sum
FROM sales ORDER BY id;

DROP TABLE sales;

SELECT 'Advanced window functions tests completed' AS result;

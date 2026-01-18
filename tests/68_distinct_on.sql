-- DISTINCT ON Tests

DROP TABLE IF EXISTS sales CASCADE;

CREATE TABLE sales (
    id INT PRIMARY KEY,
    product TEXT NOT NULL,
    region TEXT NOT NULL,
    amount INT NOT NULL,
    sale_date DATE NOT NULL
);

INSERT INTO sales VALUES 
    (1, 'Widget', 'North', 100, '2024-01-15'),
    (2, 'Widget', 'North', 150, '2024-01-20'),
    (3, 'Widget', 'South', 200, '2024-01-18'),
    (4, 'Gadget', 'North', 300, '2024-01-10'),
    (5, 'Gadget', 'North', 250, '2024-01-25'),
    (6, 'Gadget', 'South', 175, '2024-01-22');

SELECT DISTINCT ON (product) product, region, amount
FROM sales
ORDER BY product, amount DESC;

SELECT DISTINCT ON (region) region, product, amount
FROM sales
ORDER BY region, amount DESC;

SELECT DISTINCT ON (product, region) product, region, amount, sale_date
FROM sales
ORDER BY product, region, sale_date DESC;

SELECT DISTINCT product FROM sales ORDER BY product;

SELECT DISTINCT product, region FROM sales ORDER BY product, region;

DROP TABLE sales;

SELECT 'DISTINCT ON tests completed' AS result;

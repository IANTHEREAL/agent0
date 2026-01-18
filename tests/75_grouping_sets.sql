-- GROUPING SETS, CUBE, ROLLUP Tests

DROP TABLE IF EXISTS sales CASCADE;

CREATE TABLE sales (
    id INT PRIMARY KEY,
    region TEXT NOT NULL,
    product TEXT NOT NULL,
    year INT NOT NULL,
    amount INT NOT NULL
);

INSERT INTO sales VALUES
    (1, 'North', 'Widget', 2023, 1000),
    (2, 'North', 'Widget', 2024, 1200),
    (3, 'North', 'Gadget', 2023, 800),
    (4, 'North', 'Gadget', 2024, 950),
    (5, 'South', 'Widget', 2023, 1100),
    (6, 'South', 'Widget', 2024, 1300),
    (7, 'South', 'Gadget', 2023, 700),
    (8, 'South', 'Gadget', 2024, 850);

SELECT region, product, SUM(amount) AS total
FROM sales
GROUP BY GROUPING SETS ((region), (product), ())
ORDER BY region NULLS LAST, product NULLS LAST;

SELECT region, product, SUM(amount) AS total
FROM sales
GROUP BY ROLLUP (region, product)
ORDER BY region NULLS LAST, product NULLS LAST;

SELECT region, product, SUM(amount) AS total
FROM sales
GROUP BY CUBE (region, product)
ORDER BY region NULLS LAST, product NULLS LAST;

SELECT 
    region, 
    product, 
    year,
    SUM(amount) AS total,
    GROUPING(region) AS grp_region,
    GROUPING(product) AS grp_product,
    GROUPING(year) AS grp_year
FROM sales
GROUP BY CUBE (region, product, year)
HAVING region IS NULL OR product IS NULL
ORDER BY region NULLS LAST, product NULLS LAST, year NULLS LAST;

DROP TABLE sales;

SELECT 'Grouping sets tests completed' AS result;

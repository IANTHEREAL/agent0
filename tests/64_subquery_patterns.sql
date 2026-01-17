-- Subquery Patterns Test

DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS orders;

CREATE TABLE products (
    id INT PRIMARY KEY,
    name TEXT,
    price NUMERIC(10,2),
    category TEXT
);

CREATE TABLE orders (
    id INT PRIMARY KEY,
    product_id INT,
    quantity INT,
    order_date DATE
);

INSERT INTO products VALUES (1, 'Apple', 1.50, 'Fruit');
INSERT INTO products VALUES (2, 'Banana', 0.75, 'Fruit');
INSERT INTO products VALUES (3, 'Carrot', 0.50, 'Vegetable');
INSERT INTO products VALUES (4, 'Milk', 2.00, 'Dairy');
INSERT INTO products VALUES (5, 'Cheese', 5.00, 'Dairy');

INSERT INTO orders VALUES (1, 1, 10, '2024-01-15');
INSERT INTO orders VALUES (2, 2, 20, '2024-01-16');
INSERT INTO orders VALUES (3, 1, 5, '2024-01-17');
INSERT INTO orders VALUES (4, 4, 3, '2024-01-18');

-- Scalar subquery in SELECT
SELECT name, price, (SELECT AVG(price) FROM products) AS avg_price FROM products ORDER BY id;

-- Scalar subquery in WHERE
SELECT name, price FROM products WHERE price > (SELECT AVG(price) FROM products) ORDER BY id;

-- IN subquery
SELECT name FROM products WHERE id IN (SELECT product_id FROM orders) ORDER BY id;

-- NOT IN subquery
SELECT name FROM products WHERE id NOT IN (SELECT product_id FROM orders) ORDER BY id;

-- EXISTS subquery
SELECT p.name FROM products p WHERE EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.id) ORDER BY p.id;

-- NOT EXISTS subquery
SELECT p.name FROM products p WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.product_id = p.id) ORDER BY p.id;

-- Correlated subquery
SELECT p.name, (SELECT SUM(o.quantity) FROM orders o WHERE o.product_id = p.id) AS total_ordered
FROM products p ORDER BY p.id;

-- Subquery in FROM (derived table)
SELECT category, avg_price FROM (
    SELECT category, AVG(price) AS avg_price FROM products GROUP BY category
) AS cat_avg ORDER BY category;

-- Multiple levels of nesting
SELECT name FROM products WHERE price > (
    SELECT AVG(price) FROM products WHERE category = (
        SELECT category FROM products WHERE name = 'Apple'
    )
) ORDER BY id;

-- Subquery with ALL/ANY comparison operators
SELECT name FROM products WHERE price >= ALL (SELECT price FROM products WHERE category = 'Fruit') ORDER BY id;
SELECT name FROM products WHERE price > ANY (SELECT price FROM products WHERE category = 'Vegetable') ORDER BY id;

DROP TABLE orders;
DROP TABLE products;

SELECT 'Subquery patterns tests completed' AS result;

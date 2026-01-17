DROP TABLE IF EXISTS order_items CASCADE;
DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS products CASCADE;
DROP TABLE IF EXISTS customers CASCADE;

CREATE TABLE customers (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    city TEXT
);

CREATE TABLE products (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    price DECIMAL(10,2)
);

CREATE TABLE orders (
    id INT PRIMARY KEY,
    customer_id INT,
    order_date DATE
);

CREATE TABLE order_items (
    id INT PRIMARY KEY,
    order_id INT,
    product_id INT,
    quantity INT
);

INSERT INTO customers VALUES (1, 'Alice', 'NYC');
INSERT INTO customers VALUES (2, 'Bob', 'LA');
INSERT INTO customers VALUES (3, 'Charlie', 'Chicago');
INSERT INTO customers VALUES (4, 'Diana', NULL);

INSERT INTO products VALUES (1, 'Widget', 10.00);
INSERT INTO products VALUES (2, 'Gadget', 25.00);
INSERT INTO products VALUES (3, 'Gizmo', 15.00);

INSERT INTO orders VALUES (1, 1, '2024-01-15');
INSERT INTO orders VALUES (2, 1, '2024-02-20');
INSERT INTO orders VALUES (3, 2, '2024-01-25');
INSERT INTO orders VALUES (4, NULL, '2024-03-01');

INSERT INTO order_items VALUES (1, 1, 1, 2);
INSERT INTO order_items VALUES (2, 1, 2, 1);
INSERT INTO order_items VALUES (3, 2, 1, 3);
INSERT INTO order_items VALUES (4, 3, 3, 5);
INSERT INTO order_items VALUES (5, 4, 1, 1);

SELECT c.name, o.id AS order_id, o.order_date
FROM customers c
INNER JOIN orders o ON c.id = o.customer_id
ORDER BY c.name, o.id;

SELECT c.name, o.id AS order_id
FROM customers c
LEFT JOIN orders o ON c.id = o.customer_id
ORDER BY c.name, o.id NULLS LAST;

SELECT c.name, o.id AS order_id
FROM customers c
RIGHT JOIN orders o ON c.id = o.customer_id
ORDER BY o.id;

SELECT c.name, o.id AS order_id
FROM customers c
FULL OUTER JOIN orders o ON c.id = o.customer_id
ORDER BY COALESCE(c.name, ''), COALESCE(o.id, 0);

SELECT c.name, p.name AS product_name
FROM customers c
CROSS JOIN products p
WHERE c.id = 1
ORDER BY p.name;

SELECT c.name, o.id AS order_id, oi.product_id, p.name AS product_name, oi.quantity
FROM customers c
INNER JOIN orders o ON c.id = o.customer_id
INNER JOIN order_items oi ON o.id = oi.order_id
INNER JOIN products p ON oi.product_id = p.id
ORDER BY c.name, o.id, p.name;

SELECT c.name, COUNT(o.id) AS order_count, COALESCE(SUM(oi.quantity), 0) AS total_items
FROM customers c
LEFT JOIN orders o ON c.id = o.customer_id
LEFT JOIN order_items oi ON o.id = oi.order_id
GROUP BY c.id, c.name
ORDER BY c.name;

SELECT c.name
FROM customers c
WHERE EXISTS (
    SELECT 1 FROM orders o WHERE o.customer_id = c.id
)
ORDER BY c.name;

SELECT c.name
FROM customers c
WHERE NOT EXISTS (
    SELECT 1 FROM orders o WHERE o.customer_id = c.id
)
ORDER BY c.name;

SELECT c.name
FROM customers c
WHERE c.id IN (SELECT customer_id FROM orders WHERE customer_id IS NOT NULL)
ORDER BY c.name;

DROP TABLE order_items;
DROP TABLE orders;
DROP TABLE products;
DROP TABLE customers;

SELECT 'JOIN tests completed' AS result;

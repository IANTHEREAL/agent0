-- Test JOIN ... USING syntax
DROP TABLE IF EXISTS t2 CASCADE;
DROP TABLE IF EXISTS t1 CASCADE;

CREATE TABLE t1 (
    id INT PRIMARY KEY,
    name TEXT,
    category_id INT
);

CREATE TABLE t2 (
    category_id INT PRIMARY KEY,
    category_name TEXT
);

INSERT INTO t1 VALUES (1, 'Item A', 10);
INSERT INTO t1 VALUES (2, 'Item B', 20);
INSERT INTO t1 VALUES (3, 'Item C', 10);
INSERT INTO t1 VALUES (4, 'Item D', NULL);

INSERT INTO t2 VALUES (10, 'Electronics');
INSERT INTO t2 VALUES (20, 'Books');
INSERT INTO t2 VALUES (30, 'Clothing');

-- Basic INNER JOIN USING
SELECT t1.id, t1.name, t2.category_name
FROM t1
JOIN t2 USING (category_id)
ORDER BY t1.id;

-- LEFT JOIN USING
SELECT t1.id, t1.name, t2.category_name
FROM t1
LEFT JOIN t2 USING (category_id)
ORDER BY t1.id;

-- Multiple columns USING (need tables with multiple shared columns)
DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS order_details CASCADE;

CREATE TABLE orders (
    order_id INT,
    customer_id INT,
    order_date DATE,
    PRIMARY KEY (order_id, customer_id)
);

CREATE TABLE order_details (
    order_id INT,
    customer_id INT,
    product TEXT,
    quantity INT
);

INSERT INTO orders VALUES (1, 100, '2024-01-15');
INSERT INTO orders VALUES (2, 100, '2024-02-20');
INSERT INTO orders VALUES (3, 200, '2024-01-25');

INSERT INTO order_details VALUES (1, 100, 'Widget', 5);
INSERT INTO order_details VALUES (1, 100, 'Gadget', 2);
INSERT INTO order_details VALUES (2, 100, 'Widget', 3);
INSERT INTO order_details VALUES (3, 200, 'Gizmo', 1);
INSERT INTO order_details VALUES (4, 300, 'Widget', 10);

-- JOIN with multiple USING columns
SELECT o.order_date, d.product, d.quantity
FROM orders o
JOIN order_details d USING (order_id, customer_id)
ORDER BY o.order_date, d.product;

-- Cleanup
DROP TABLE IF EXISTS order_details CASCADE;
DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS t2 CASCADE;
DROP TABLE IF EXISTS t1 CASCADE;

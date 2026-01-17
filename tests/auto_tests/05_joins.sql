-- Auto tests: JOINs
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS join_orders;
DROP TABLE IF EXISTS join_customers;
DROP TABLE IF EXISTS join_left;
DROP TABLE IF EXISTS join_right;

CREATE TABLE join_customers (
    id INT PRIMARY KEY,
    name TEXT
);

CREATE TABLE join_orders (
    id INT PRIMARY KEY,
    customer_id INT,
    total INT
);

INSERT INTO join_customers (id, name) VALUES
    (1, 'alice'),
    (2, 'bob'),
    (3, 'carol');

INSERT INTO join_orders (id, customer_id, total) VALUES
    (10, 1, 100),
    (11, 1, 200),
    (12, 2, 300);

SELECT c.name, o.total
FROM join_customers c
INNER JOIN join_orders o ON c.id = o.customer_id
ORDER BY o.id;

SELECT c.name, o.total
FROM join_customers c
LEFT JOIN join_orders o ON c.id = o.customer_id
ORDER BY c.id, o.id;

SELECT c.name, o.total
FROM join_customers c
RIGHT JOIN join_orders o ON c.id = o.customer_id
ORDER BY o.id;

SELECT c.name, o.total
FROM join_customers c
FULL OUTER JOIN join_orders o ON c.id = o.customer_id
ORDER BY c.id, o.id;

CREATE TABLE join_left (id INT, value TEXT);
CREATE TABLE join_right (id INT, note TEXT);
INSERT INTO join_left (id, value) VALUES (1, 'l1'), (2, 'l2');
INSERT INTO join_right (id, note) VALUES (1, 'r1');

SELECT * FROM join_left NATURAL JOIN join_right ORDER BY id;
SELECT * FROM join_left CROSS JOIN join_right ORDER BY join_left.id, join_right.id;

DROP TABLE join_orders;
DROP TABLE join_customers;
DROP TABLE join_left;
DROP TABLE join_right;

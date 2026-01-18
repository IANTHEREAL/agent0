DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS customers CASCADE;

CREATE TABLE customers (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    credit INT DEFAULT 0
);

CREATE TABLE orders (
    id INT PRIMARY KEY,
    customer_id INT REFERENCES customers(id),
    amount INT NOT NULL,
    status TEXT DEFAULT 'pending'
);

INSERT INTO customers VALUES (1, 'Alice', 1000), (2, 'Bob', 500), (3, 'Charlie', 0);
INSERT INTO orders VALUES 
    (1, 1, 100, 'pending'),
    (2, 1, 200, 'pending'),
    (3, 2, 150, 'shipped'),
    (4, 2, 300, 'pending'),
    (5, 3, 50, 'pending');

UPDATE orders SET status = 'processing' WHERE id = 1;
SELECT * FROM orders WHERE id = 1;

UPDATE orders SET status = 'shipped', amount = amount + 10 WHERE customer_id = 1;
SELECT * FROM orders WHERE customer_id = 1 ORDER BY id;

UPDATE orders SET status = 'confirmed' WHERE id = 3 RETURNING *;
UPDATE orders SET amount = amount * 2 WHERE id = 4 RETURNING id, amount AS new_amount;

UPDATE orders o
SET status = 'vip_order'
FROM customers c
WHERE o.customer_id = c.id AND c.credit > 800;
SELECT * FROM orders WHERE status = 'vip_order' ORDER BY id;

UPDATE customers
SET credit = credit + 100
WHERE id IN (SELECT DISTINCT customer_id FROM orders WHERE amount > 100)
RETURNING *;

DELETE FROM orders WHERE id = 5;
SELECT COUNT(*) AS remaining FROM orders;

DELETE FROM orders WHERE id = 4 RETURNING *;

DELETE FROM orders o
USING customers c
WHERE o.customer_id = c.id AND c.name = 'Alice';
SELECT * FROM orders ORDER BY id;

DELETE FROM orders WHERE customer_id = (SELECT id FROM customers WHERE name = 'Bob') RETURNING id;

DROP TABLE orders;
DROP TABLE customers;

SELECT 'UPDATE/DELETE variants tests completed' AS result;

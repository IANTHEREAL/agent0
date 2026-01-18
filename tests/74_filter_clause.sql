-- Aggregate FILTER Clause Tests

DROP TABLE IF EXISTS orders CASCADE;

CREATE TABLE orders (
    id INT PRIMARY KEY,
    customer_id INT NOT NULL,
    amount INT NOT NULL,
    status TEXT NOT NULL,
    order_date DATE NOT NULL
);

INSERT INTO orders VALUES
    (1, 1, 100, 'completed', '2024-01-15'),
    (2, 1, 200, 'completed', '2024-01-20'),
    (3, 1, 50, 'cancelled', '2024-01-22'),
    (4, 2, 300, 'completed', '2024-01-10'),
    (5, 2, 150, 'pending', '2024-01-25'),
    (6, 3, 75, 'completed', '2024-01-18'),
    (7, 3, 125, 'cancelled', '2024-01-28');

SELECT 
    COUNT(*) AS total_orders,
    COUNT(*) FILTER (WHERE status = 'completed') AS completed_orders,
    COUNT(*) FILTER (WHERE status = 'cancelled') AS cancelled_orders,
    COUNT(*) FILTER (WHERE status = 'pending') AS pending_orders
FROM orders;

SELECT 
    SUM(amount) AS total_amount,
    SUM(amount) FILTER (WHERE status = 'completed') AS completed_amount,
    SUM(amount) FILTER (WHERE status != 'cancelled') AS non_cancelled_amount
FROM orders;

SELECT 
    AVG(amount) AS overall_avg,
    AVG(amount) FILTER (WHERE status = 'completed') AS completed_avg
FROM orders;

SELECT 
    customer_id,
    COUNT(*) AS total,
    COUNT(*) FILTER (WHERE status = 'completed') AS completed,
    SUM(amount) FILTER (WHERE status = 'completed') AS completed_sum
FROM orders
GROUP BY customer_id
ORDER BY customer_id;

SELECT 
    customer_id,
    COUNT(*) FILTER (WHERE amount > 100) AS large_orders,
    COUNT(*) FILTER (WHERE amount <= 100) AS small_orders
FROM orders
GROUP BY customer_id
ORDER BY customer_id;

DROP TABLE orders;

SELECT 'FILTER clause tests completed' AS result;

-- Scalar subquery nested inside aggregate function
-- Regression test for issue #2230:
--   SUM(COALESCE((SELECT ...), 0)) fails with "aggregate function 'SUM'
--   cannot be evaluated per-row" because the deferred-async path pulls the
--   entire expression out of the aggregate operator.

DROP TABLE IF EXISTS agg_sub_items CASCADE;
DROP TABLE IF EXISTS agg_sub_orders CASCADE;

CREATE TABLE agg_sub_orders (
    id SERIAL PRIMARY KEY,
    customer_id INT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE agg_sub_items (
    id SERIAL PRIMARY KEY,
    order_id INT NOT NULL REFERENCES agg_sub_orders(id),
    product TEXT NOT NULL,
    quantity INT NOT NULL DEFAULT 1,
    price NUMERIC(10,2) NOT NULL
);

INSERT INTO agg_sub_orders (customer_id, status) VALUES
    (1, 'completed'),
    (1, 'pending'),
    (2, 'completed'),
    (3, 'completed');

INSERT INTO agg_sub_items (order_id, product, quantity, price) VALUES
    (1, 'Widget', 2, 10.00),
    (1, 'Gadget', 1, 25.00),
    (3, 'Widget', 5, 10.00),
    (4, 'Gadget', 3, 25.00);

-- ============================================================
-- Baseline: scalar subquery without outer aggregate (works)
-- ============================================================

-- Q1: COALESCE + scalar subquery per row — this works today
SELECT
    o.id,
    COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    ) AS order_total
FROM agg_sub_orders o
ORDER BY o.id;

-- ============================================================
-- Bug: scalar subquery wrapped in outer aggregate
-- ============================================================

-- Q2: SUM(COALESCE(scalar_subquery, 0)) GROUP BY
-- Fails: "aggregate function 'SUM' cannot be evaluated per-row"
SELECT
    o.customer_id,
    SUM(COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    )) AS total_spent
FROM agg_sub_orders o
WHERE o.status = 'completed'
GROUP BY o.customer_id
ORDER BY o.customer_id;

-- Q3: COUNT with scalar subquery filter
SELECT
    o.customer_id,
    COUNT(*) AS order_count,
    SUM(COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    )) AS total_spent
FROM agg_sub_orders o
GROUP BY o.customer_id
ORDER BY o.customer_id;

-- Q4: scalar subquery directly inside SUM (no COALESCE)
SELECT
    o.customer_id,
    SUM(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id)
    ) AS total_spent
FROM agg_sub_orders o
WHERE o.status = 'completed'
GROUP BY o.customer_id
ORDER BY o.customer_id;

-- Q5: AVG of scalar subquery
SELECT
    o.customer_id,
    AVG(COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    )) AS avg_order_value
FROM agg_sub_orders o
WHERE o.status = 'completed'
GROUP BY o.customer_id
ORDER BY o.customer_id;

-- Q6: MIN/MAX of scalar subquery
SELECT
    o.customer_id,
    MIN((SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id)) AS min_order,
    MAX((SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id)) AS max_order
FROM agg_sub_orders o
GROUP BY o.customer_id
ORDER BY o.customer_id;

-- ============================================================
-- HAVING with scalar subquery in aggregate
-- ============================================================

-- Q7: HAVING clause filtering on aggregate with scalar subquery
SELECT
    o.customer_id,
    SUM(COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    )) AS total_spent
FROM agg_sub_orders o
GROUP BY o.customer_id
HAVING SUM(COALESCE(
    (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
    0
)) > 50
ORDER BY o.customer_id;

-- ============================================================
-- Whole-table aggregate (no GROUP BY)
-- ============================================================

-- Q8: SUM over all rows without GROUP BY
SELECT
    SUM(COALESCE(
        (SELECT SUM(i.price * i.quantity) FROM agg_sub_items i WHERE i.order_id = o.id),
        0
    )) AS grand_total
FROM agg_sub_orders o
WHERE o.status = 'completed';

-- ============================================================
-- Cleanup
-- ============================================================

DROP TABLE agg_sub_items;
DROP TABLE agg_sub_orders;

-- GENERATE_SERIES Tests

SET TIME ZONE 'America/Los_Angeles';

SELECT * FROM generate_series(1, 5) AS n ORDER BY n;

SELECT * FROM generate_series(0, 10, 2) AS n ORDER BY n;

SELECT * FROM generate_series(5, 1, -1) AS n ORDER BY n;

SELECT * FROM generate_series(1.0, 2.0, 0.25) AS n ORDER BY n;

SELECT * FROM generate_series(
    '2024-01-01'::DATE,
    '2024-01-05'::DATE,
    '1 day'::INTERVAL
) AS d ORDER BY d;

SELECT * FROM generate_series(
    '2024-01-01 00:00:00'::TIMESTAMP,
    '2024-01-01 03:00:00'::TIMESTAMP,
    '1 hour'::INTERVAL
) AS ts ORDER BY ts;

SELECT n, n * n AS square FROM generate_series(1, 5) AS n ORDER BY n;

SELECT
    d::DATE AS date,
    EXTRACT(DOW FROM d) AS day_of_week
FROM generate_series('2024-01-01'::DATE, '2024-01-07'::DATE, '1 day'::INTERVAL) AS d
ORDER BY d;

DROP TABLE IF EXISTS orders CASCADE;
CREATE TABLE orders (
    id INT PRIMARY KEY,
    order_date DATE NOT NULL,
    amount INT NOT NULL
);

INSERT INTO orders VALUES (1, '2024-01-02', 100);
INSERT INTO orders VALUES (2, '2024-01-02', 200);
INSERT INTO orders VALUES (3, '2024-01-04', 150);

SELECT
    dates.d::DATE AS date,
    COALESCE(SUM(o.amount), 0) AS total
FROM generate_series('2024-01-01'::DATE, '2024-01-05'::DATE, '1 day'::INTERVAL) AS dates(d)
LEFT JOIN orders o ON o.order_date = dates.d::DATE
GROUP BY dates.d
ORDER BY dates.d;

DROP TABLE orders;

SELECT 'GENERATE_SERIES tests completed' AS result;

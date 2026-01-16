-- NUMERIC/DECIMAL type integration tests

-- ============================================
-- 1. Basic DDL and Type Definitions
-- ============================================

DROP TABLE IF EXISTS numeric_test;

CREATE TABLE numeric_test (
    id INT PRIMARY KEY,
    price DECIMAL(10,2),
    quantity NUMERIC(5,0),
    rate NUMERIC,
    small_val NUMERIC(3,2),
    big_val NUMERIC(20,5)
);

-- Verify schema in information_schema
SELECT column_name, data_type, numeric_precision, numeric_scale
FROM information_schema.columns
WHERE table_name = 'numeric_test'
ORDER BY ordinal_position;

-- ============================================
-- 2. Basic INSERT and SELECT
-- ============================================

INSERT INTO numeric_test VALUES (1, 123.45, 100, 0.15, 1.23, 12345678901234.56789);
INSERT INTO numeric_test VALUES (2, 999.99, 50, 0.25, 9.99, 99999999999999.99999);
INSERT INTO numeric_test VALUES (3, 0.01, 1000, 1.5, 0.01, 0.00001);
INSERT INTO numeric_test VALUES (4, -123.45, -100, -0.15, -1.23, -12345678901234.56789);

SELECT * FROM numeric_test ORDER BY id;

-- ============================================
-- 3. Comparison Operations
-- ============================================

-- Basic comparisons
SELECT id, price FROM numeric_test WHERE price > 100 ORDER BY id;
SELECT id, price FROM numeric_test WHERE price < 0 ORDER BY id;
SELECT id, price FROM numeric_test WHERE price = 123.45 ORDER BY id;
SELECT id, price FROM numeric_test WHERE price != 999.99 ORDER BY id;
SELECT id, price FROM numeric_test WHERE price >= 0.01 AND price <= 200 ORDER BY id;
SELECT id, price FROM numeric_test WHERE price BETWEEN -200 AND 200 ORDER BY id;

-- Comparison with integers
SELECT id FROM numeric_test WHERE price > 100 ORDER BY id;
SELECT id FROM numeric_test WHERE quantity = 100 ORDER BY id;

-- ============================================
-- 4. Arithmetic Operations
-- ============================================

-- Addition
SELECT id, price + 10 AS plus_ten FROM numeric_test ORDER BY id;
SELECT id, price + rate AS price_plus_rate FROM numeric_test ORDER BY id;
SELECT id, price + quantity AS price_plus_qty FROM numeric_test ORDER BY id;

-- Subtraction
SELECT id, price - 10 AS minus_ten FROM numeric_test ORDER BY id;
SELECT id, price - rate AS price_minus_rate FROM numeric_test ORDER BY id;

-- Multiplication
SELECT id, price * 2 AS doubled FROM numeric_test ORDER BY id;
SELECT id, price * quantity AS total FROM numeric_test ORDER BY id;
SELECT id, price * rate AS adjusted FROM numeric_test ORDER BY id;

-- Division
SELECT id, price / 2 AS halved FROM numeric_test ORDER BY id;
SELECT id, price / rate AS ratio FROM numeric_test WHERE rate != 0 ORDER BY id;

-- Mixed arithmetic
SELECT id, (price * quantity) + (price * rate) AS complex_calc FROM numeric_test ORDER BY id;

-- ============================================
-- 5. Aggregate Functions
-- ============================================

SELECT SUM(price) AS total_price FROM numeric_test;
SELECT AVG(price) AS avg_price FROM numeric_test;
SELECT MIN(price) AS min_price FROM numeric_test;
SELECT MAX(price) AS max_price FROM numeric_test;
SELECT COUNT(*) AS cnt, SUM(price) AS total FROM numeric_test WHERE price > 0;

-- Aggregates with GROUP BY
SELECT 
    CASE WHEN price >= 0 THEN 'positive' ELSE 'negative' END AS sign,
    COUNT(*) AS cnt,
    SUM(price) AS total,
    AVG(price) AS average
FROM numeric_test
GROUP BY CASE WHEN price >= 0 THEN 'positive' ELSE 'negative' END
ORDER BY sign;

-- ============================================
-- 6. ORDER BY with NUMERIC
-- ============================================

SELECT id, price FROM numeric_test ORDER BY price ASC;
SELECT id, price FROM numeric_test ORDER BY price DESC;
SELECT id, big_val FROM numeric_test ORDER BY big_val ASC;

-- ============================================
-- 7. Type Coercion and CAST
-- ============================================

-- Cast from text
SELECT CAST('123.456' AS NUMERIC) AS from_text;
SELECT CAST('123.456' AS NUMERIC(10,2)) AS from_text_with_scale;
SELECT CAST('-999.99' AS DECIMAL(6,2)) AS negative_from_text;

-- Cast from integer
SELECT CAST(42 AS NUMERIC) AS from_int;
SELECT CAST(42 AS NUMERIC(5,2)) AS from_int_with_scale;

-- Cast from float
SELECT CAST(3.14159 AS NUMERIC(10,4)) AS from_float;

-- Cast to other types
SELECT CAST(price AS INTEGER) AS to_int FROM numeric_test WHERE id = 1;
SELECT CAST(price AS BIGINT) AS to_bigint FROM numeric_test WHERE id = 1;
SELECT CAST(price AS TEXT) AS to_text FROM numeric_test WHERE id = 1;

-- ============================================
-- 8. Precision and Scale Handling
-- ============================================

-- Scale truncation (rounding)
DROP TABLE IF EXISTS scale_test;
CREATE TABLE scale_test (id INT PRIMARY KEY, val NUMERIC(5,2));
INSERT INTO scale_test VALUES (1, 1.234);  -- Should round to 1.23
INSERT INTO scale_test VALUES (2, 1.235);  -- Should round to 1.24
INSERT INTO scale_test VALUES (3, 9.999);  -- Should round to 10.00
SELECT * FROM scale_test ORDER BY id;
DROP TABLE scale_test;

-- ============================================
-- 9. NULL Handling
-- ============================================

DROP TABLE IF EXISTS null_numeric;
CREATE TABLE null_numeric (id INT PRIMARY KEY, val NUMERIC(10,2));
INSERT INTO null_numeric VALUES (1, 100.00);
INSERT INTO null_numeric VALUES (2, NULL);
INSERT INTO null_numeric VALUES (3, 200.00);

SELECT * FROM null_numeric ORDER BY id;
SELECT id FROM null_numeric WHERE val IS NULL;
SELECT id FROM null_numeric WHERE val IS NOT NULL ORDER BY id;
SELECT SUM(val) AS total, AVG(val) AS average FROM null_numeric;
SELECT COALESCE(val, 0) AS val_or_zero FROM null_numeric ORDER BY id;

DROP TABLE null_numeric;

-- ============================================
-- 10. Index Operations with NUMERIC
-- ============================================

DROP TABLE IF EXISTS numeric_indexed;
CREATE TABLE numeric_indexed (
    id INT PRIMARY KEY,
    price NUMERIC(10,2)
);
CREATE INDEX idx_numeric_price ON numeric_indexed(price);

INSERT INTO numeric_indexed VALUES (1, 10.00);
INSERT INTO numeric_indexed VALUES (2, 20.00);
INSERT INTO numeric_indexed VALUES (3, 15.00);
INSERT INTO numeric_indexed VALUES (4, 5.00);
INSERT INTO numeric_indexed VALUES (5, 25.00);

-- These should use index scan
SELECT id, price FROM numeric_indexed WHERE price = 15.00;
SELECT id, price FROM numeric_indexed WHERE price > 15.00 ORDER BY price;
SELECT id, price FROM numeric_indexed WHERE price BETWEEN 10.00 AND 20.00 ORDER BY price;

DROP TABLE numeric_indexed;

-- ============================================
-- 11. NUMERIC as Primary Key
-- ============================================

DROP TABLE IF EXISTS numeric_pk;
CREATE TABLE numeric_pk (
    code NUMERIC(10,2) PRIMARY KEY,
    name TEXT
);

INSERT INTO numeric_pk VALUES (100.00, 'hundred');
INSERT INTO numeric_pk VALUES (100.50, 'hundred-fifty');
INSERT INTO numeric_pk VALUES (99.99, 'ninety-nine');

SELECT * FROM numeric_pk ORDER BY code;
SELECT * FROM numeric_pk WHERE code = 100.00;

DROP TABLE numeric_pk;

-- ============================================
-- 12. Math Functions with NUMERIC
-- ============================================

SELECT ABS(CAST(-123.45 AS NUMERIC)) AS abs_val;
SELECT CEIL(CAST(123.45 AS NUMERIC)) AS ceil_val;
SELECT FLOOR(CAST(123.45 AS NUMERIC)) AS floor_val;
SELECT ROUND(CAST(123.456 AS NUMERIC), 2) AS round_val;
SELECT SQRT(CAST(144 AS NUMERIC)) AS sqrt_val;
SELECT POWER(CAST(2 AS NUMERIC), CAST(10 AS NUMERIC)) AS power_val;
SELECT SIGN(CAST(-123.45 AS NUMERIC)) AS sign_neg;
SELECT SIGN(CAST(123.45 AS NUMERIC)) AS sign_pos;
SELECT SIGN(CAST(0 AS NUMERIC)) AS sign_zero;

-- ============================================
-- 13. NUMERIC in Subqueries
-- ============================================

SELECT id, price FROM numeric_test 
WHERE price > (SELECT AVG(price) FROM numeric_test)
ORDER BY id;

SELECT id, price FROM numeric_test 
WHERE price = (SELECT MAX(price) FROM numeric_test);

SELECT id, price FROM numeric_test 
WHERE price IN (SELECT price FROM numeric_test WHERE quantity > 50)
ORDER BY id;

-- ============================================
-- 14. NUMERIC in JOINs
-- ============================================

DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS discounts;

CREATE TABLE products (
    id INT PRIMARY KEY,
    name TEXT,
    price NUMERIC(10,2)
);

CREATE TABLE discounts (
    product_id INT,
    discount_rate NUMERIC(5,2)
);

INSERT INTO products VALUES (1, 'Widget', 100.00);
INSERT INTO products VALUES (2, 'Gadget', 250.00);
INSERT INTO products VALUES (3, 'Gizmo', 75.50);

INSERT INTO discounts VALUES (1, 10.00);
INSERT INTO discounts VALUES (2, 15.00);
INSERT INTO discounts VALUES (3, 5.00);

SELECT p.name, p.price, d.discount_rate, 
       p.price * (1 - d.discount_rate / 100) AS discounted_price
FROM products p
JOIN discounts d ON p.id = d.product_id
ORDER BY p.id;

DROP TABLE products;
DROP TABLE discounts;

-- ============================================
-- 15. NUMERIC in CASE Expressions
-- ============================================

SELECT id, price,
    CASE 
        WHEN price < 0 THEN 'negative'
        WHEN price = 0 THEN 'zero'
        WHEN price < 100 THEN 'low'
        WHEN price < 500 THEN 'medium'
        ELSE 'high'
    END AS price_category
FROM numeric_test
ORDER BY id;

-- ============================================
-- 16. NUMERIC with DISTINCT
-- ============================================

DROP TABLE IF EXISTS dup_numeric;
CREATE TABLE dup_numeric (id INT PRIMARY KEY, val NUMERIC(10,2));
INSERT INTO dup_numeric VALUES (1, 100.00);
INSERT INTO dup_numeric VALUES (2, 100.00);
INSERT INTO dup_numeric VALUES (3, 200.00);
INSERT INTO dup_numeric VALUES (4, 100.00);

SELECT DISTINCT val FROM dup_numeric ORDER BY val;
SELECT COUNT(DISTINCT val) AS unique_count FROM dup_numeric;

DROP TABLE dup_numeric;

-- ============================================
-- 17. Edge Cases
-- ============================================

-- Zero values
SELECT CAST(0 AS NUMERIC) AS zero;
SELECT CAST(0.00 AS NUMERIC(5,2)) AS zero_with_scale;
SELECT CAST(-0.00 AS NUMERIC(5,2)) AS neg_zero;

-- Very small values
SELECT CAST(0.0000001 AS NUMERIC(10,7)) AS tiny;

-- Decimal literal parsing
SELECT 3.14159265358979 AS pi_approx;
SELECT -2.71828 AS neg_e;

-- ============================================
-- 18. COPY Support
-- ============================================

DROP TABLE IF EXISTS copy_numeric;
CREATE TABLE copy_numeric (id INT PRIMARY KEY, val NUMERIC(10,2));
INSERT INTO copy_numeric VALUES (1, 123.45);
INSERT INTO copy_numeric VALUES (2, -678.90);
SELECT * FROM copy_numeric ORDER BY id;
DROP TABLE copy_numeric;

-- ============================================
-- Cleanup
-- ============================================

DROP TABLE IF EXISTS numeric_test;

SELECT 'All NUMERIC/DECIMAL tests completed successfully!' AS result;

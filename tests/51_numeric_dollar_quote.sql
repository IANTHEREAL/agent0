-- NUMERIC with dollar-quoted strings integration tests

DROP TABLE IF EXISTS numeric_dq_test;

CREATE TABLE numeric_dq_test (
    id INT PRIMARY KEY,
    price DECIMAL(10,2),
    description TEXT
);

INSERT INTO numeric_dq_test VALUES (1, 123.45, $$Product with price 123.45$$);
INSERT INTO numeric_dq_test VALUES (2, 999.99, $$Special item worth 999.99$$);
INSERT INTO numeric_dq_test VALUES (3, 0.01, $$Tiny price: $0.01$$);

SELECT id, price, description FROM numeric_dq_test ORDER BY id;

SELECT id, price, $$Price is: $$ || CAST(price AS TEXT) AS formatted FROM numeric_dq_test ORDER BY id;

SELECT * FROM numeric_dq_test WHERE description LIKE $$%123%$$;

SELECT SUM(price) AS total_price FROM numeric_dq_test;

DROP TABLE IF EXISTS dq_numeric_calc;
CREATE TABLE dq_numeric_calc (id INT PRIMARY KEY, formula TEXT, val NUMERIC(10,2));
INSERT INTO dq_numeric_calc VALUES (1, $$price * 2$$, 100.00);
INSERT INTO dq_numeric_calc VALUES (2, $$val + 50$$, 200.00);
SELECT id, formula, val, val * 2 AS doubled FROM dq_numeric_calc ORDER BY id;
DROP TABLE dq_numeric_calc;

SELECT $$Result: $$ || CAST(CAST($$123.456$$ AS NUMERIC(10,2)) AS TEXT) AS cast_test;

SELECT $$Line 1
Line 2
Line 3$$ AS multiline;

SELECT id, price FROM numeric_dq_test WHERE price > 100 AND description LIKE $$%Product%$$;

SELECT $$Dollar sign in value: $$ || CAST(price AS TEXT) || $$ dollars$$ AS with_dollar 
FROM numeric_dq_test WHERE id = 3;

SELECT 
    CASE WHEN price < 1 THEN $$cheap$$ ELSE $$expensive$$ END AS category,
    price
FROM numeric_dq_test ORDER BY id;

DROP TABLE numeric_dq_test;

SELECT $$NUMERIC + Dollar-Quote tests passed!$$ AS result;

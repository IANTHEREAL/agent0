-- Decimal precision: NUMERIC values must retain exact precision through
-- insert, query, and arithmetic. No hidden float fallback.

DROP TABLE IF EXISTS t_decimal_prec CASCADE;

CREATE TABLE t_decimal_prec (
    id SERIAL PRIMARY KEY,
    amount DECIMAL(15,2) NOT NULL,
    rate DECIMAL(10,6) NOT NULL
);

INSERT INTO t_decimal_prec (amount, rate) VALUES (123456789.12, 0.123456);
INSERT INTO t_decimal_prec (amount, rate) VALUES (0.01, 9999.999999);

-- Verify exact values retained.
SELECT amount, rate FROM t_decimal_prec ORDER BY id;

-- Arithmetic preserves precision.
SELECT amount + 0.01 AS total FROM t_decimal_prec WHERE id = 1;

-- Comparison with exact values.
SELECT amount FROM t_decimal_prec WHERE amount = 0.01;

DROP TABLE t_decimal_prec CASCADE;

-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: EXP/POWER float errors consistently on/off.

DROP TABLE IF EXISTS db9_cop_exp_power_error_smoke;

CREATE TABLE db9_cop_exp_power_error_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    exp_arg DOUBLE PRECISION NOT NULL,
    exp_inf_arg DOUBLE PRECISION NOT NULL,
    power_neg_base DOUBLE PRECISION NOT NULL,
    power_fractional_exp DOUBLE PRECISION NOT NULL,
    power_big_base DOUBLE PRECISION NOT NULL,
    power_big_exp DOUBLE PRECISION NOT NULL,
    power_inf_base DOUBLE PRECISION NOT NULL
);
CREATE INDEX db9_cop_exp_power_error_smoke_n_idx ON db9_cop_exp_power_error_smoke(n);

INSERT INTO db9_cop_exp_power_error_smoke VALUES
    (1, 10, 1.0, 'Infinity'::float8, -1.0, 2.0, 10.0, 2.0, 'Infinity'::float8),
    (2, 20, 1000.0, 'Infinity'::float8, -1.0, 0.5, 10.0, 400.0, 'Infinity'::float8),
    (3, 30, 2.0, 'Infinity'::float8, -8.0, 3.0, 2.0, 8.0, 'Infinity'::float8);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT
    EXP(exp_arg) AS exp_overflow_out,
    EXP(exp_inf_arg) AS exp_infinity_out,
    POWER(power_neg_base, power_fractional_exp) AS power_domain_out,
    POWER(power_big_base, power_big_exp) AS power_overflow_out,
    POWER(power_inf_base, 2.0::float8) AS power_infinity_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    EXP(exp_arg) AS exp_overflow_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(power_neg_base, power_fractional_exp) AS power_domain_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(power_big_base, power_big_exp) AS power_overflow_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(0::float8, -1::float8) AS power_zero_negative_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    EXP(exp_inf_arg) AS exp_infinity_out,
    POWER(power_inf_base, 2.0::float8) AS power_infinity_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT
    EXP(exp_arg) AS exp_overflow_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(power_neg_base, power_fractional_exp) AS power_domain_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(power_big_base, power_big_exp) AS power_overflow_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    POWER(0::float8, -1::float8) AS power_zero_negative_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;
SELECT
    EXP(exp_inf_arg) AS exp_infinity_out,
    POWER(power_inf_base, 2.0::float8) AS power_infinity_out
FROM db9_cop_exp_power_error_smoke
WHERE n = 20
LIMIT 1;

DROP TABLE db9_cop_exp_power_error_smoke;

-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: ABS integer overflow errors consistently on/off.

DROP TABLE IF EXISTS db9_cop_abs_min_smoke;

CREATE TABLE db9_cop_abs_min_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    int_min INT NOT NULL,
    bigint_min BIGINT NOT NULL
);
CREATE INDEX db9_cop_abs_min_smoke_n_idx ON db9_cop_abs_min_smoke(n);

INSERT INTO db9_cop_abs_min_smoke VALUES
    (1, 10, -2147483648, -9223372036854775808),
    (2, 20, -2147483648, -9223372036854775808),
    (3, 30, -2147483648, -9223372036854775808);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT
    ABS(int_min) AS abs_int_min_out,
    ABS(bigint_min) AS abs_bigint_min_out
FROM db9_cop_abs_min_smoke
WHERE n = 20
LIMIT 1;
SELECT
    ABS(int_min) AS abs_int_min_out
FROM db9_cop_abs_min_smoke
WHERE n = 20
LIMIT 1;
SELECT
    ABS(bigint_min) AS abs_bigint_min_out
FROM db9_cop_abs_min_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT
    ABS(int_min) AS abs_int_min_out
FROM db9_cop_abs_min_smoke
WHERE n = 20
LIMIT 1;
SELECT
    ABS(bigint_min) AS abs_bigint_min_out
FROM db9_cop_abs_min_smoke
WHERE n = 20
LIMIT 1;

DROP TABLE db9_cop_abs_min_smoke;

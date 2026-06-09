-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: MOD integer errors consistently on/off.

DROP TABLE IF EXISTS db9_cop_mod_zero_smoke;

CREATE TABLE db9_cop_mod_zero_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    zero_int INT NOT NULL,
    int_min INT NOT NULL,
    bigint_min BIGINT NOT NULL,
    neg_one INT NOT NULL,
    two_big BIGINT NOT NULL
);
CREATE INDEX db9_cop_mod_zero_smoke_n_idx ON db9_cop_mod_zero_smoke(n);

INSERT INTO db9_cop_mod_zero_smoke VALUES
    (1, 10, 0, -2147483648, -9223372036854775808, -1, 2),
    (2, 21, 0, -2147483648, -9223372036854775808, -1, 2),
    (3, 30, 0, -2147483648, -9223372036854775808, -1, 2);

SET db9.enable_cop_pushdown = on;
EXPLAIN SELECT
    MOD(n, zero_int) AS mod_zero_out,
    MOD(n, two_big) AS mod_mixed_out,
    MOD(int_min, neg_one) AS mod_int_min_out,
    MOD(bigint_min, neg_one::bigint) AS mod_bigint_min_out
FROM db9_cop_mod_zero_smoke
WHERE n = 21
LIMIT 1;
SELECT
    MOD(n, two_big) AS mod_mixed_out,
    MOD(int_min, neg_one) AS mod_int_min_out,
    MOD(bigint_min, neg_one::bigint) AS mod_bigint_min_out
FROM db9_cop_mod_zero_smoke
WHERE n = 21
LIMIT 1;
SELECT
    MOD(n, zero_int) AS mod_zero_out
FROM db9_cop_mod_zero_smoke
WHERE n = 21
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT
    MOD(n, two_big) AS mod_mixed_out,
    MOD(int_min, neg_one) AS mod_int_min_out,
    MOD(bigint_min, neg_one::bigint) AS mod_bigint_min_out
FROM db9_cop_mod_zero_smoke
WHERE n = 21
LIMIT 1;
SELECT
    MOD(n, zero_int) AS mod_zero_out
FROM db9_cop_mod_zero_smoke
WHERE n = 21
LIMIT 1;

DROP TABLE db9_cop_mod_zero_smoke;

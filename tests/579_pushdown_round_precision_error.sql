-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: ROUND/TRUNC keep the PostgreSQL overload surface exact.

DROP TABLE IF EXISTS db9_cop_round_precision_smoke;

CREATE TABLE db9_cop_round_precision_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    f8 DOUBLE PRECISION NOT NULL,
    big_precision BIGINT NOT NULL
);
CREATE INDEX db9_cop_round_precision_smoke_n_idx ON db9_cop_round_precision_smoke(n);

INSERT INTO db9_cop_round_precision_smoke VALUES
    (1, 10, 12.34, 1),
    (2, 20, 12.34, 2147483648),
    (3, 30, 56.78, 2);

SET db9.enable_cop_pushdown = on;
\o /tmp/579_round_trunc_float8_explain.txt
EXPLAIN SELECT
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_round_trunc_float8_explain.txt && grep -Fq "DB9 Cop Output: round_out, trunc_out" /tmp/579_round_trunc_float8_explain.txt; then echo "round_trunc_float8_projection_pushes|1"; else echo "round_trunc_float8_projection_pushes|0"; fi
SELECT
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;

-- PostgreSQL has one-argument float8 ROUND/TRUNC and two-argument numeric,int4
-- ROUND/TRUNC. float8,bigint must fail during signature resolution instead of
-- being serialized to DB9 Cop as a remote precision-overflow case.
SELECT
    ROUND(f8, big_precision) AS round_precision_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;
SELECT
    TRUNC(f8, big_precision) AS trunc_precision_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;
SELECT
    ROUND(f8, big_precision) AS round_precision_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;
SELECT
    TRUNC(f8, big_precision) AS trunc_precision_out
FROM db9_cop_round_precision_smoke
WHERE n = 20
LIMIT 1;

\! rm -f /tmp/579_round_trunc_float8_explain.txt

DROP TABLE db9_cop_round_precision_smoke;

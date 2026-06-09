-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: unsupported operator expressions stay local and keep parity.

DROP TABLE IF EXISTS db9_cop_operator_smoke;
DROP TABLE IF EXISTS db9_cop_operator_on;
DROP TABLE IF EXISTS db9_cop_operator_off;

CREATE TABLE db9_cop_operator_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    n2 BIGINT NOT NULL,
    txt TEXT NOT NULL,
    flag BOOLEAN NOT NULL,
    maybe_flag BOOLEAN
);
CREATE INDEX db9_cop_operator_smoke_n_idx ON db9_cop_operator_smoke(n);
INSERT INTO db9_cop_operator_smoke VALUES
    (1, 10, 33, 'first', true, false),
    (2, 20, 65, 'AbC', true, NULL),
    (3, 30, 99, 'last', false, true);

SET db9.enable_cop_pushdown = on;
\o /tmp/575_pushdown_operator_expressions_explain.txt
EXPLAIN SELECT
    txt LIKE 'A%' AS like_flag,
    txt ILIKE 'a%' AS ilike_flag,
    txt NOT LIKE 'z%' AS not_like_flag,
    n BETWEEN 10 AND 20 AS between_flag,
    n NOT BETWEEN 21 AND 30 AS not_between_flag,
    n IN (10, 20, NULL) AS in_flag,
    n NOT IN (10, NULL) AS not_in_flag,
    flag IS TRUE AS is_true_flag,
    flag IS NOT FALSE AS is_not_false_flag,
    maybe_flag IS UNKNOWN AS is_unknown_flag,
    n IS DISTINCT FROM 10 AS is_distinct_flag,
    n IS NOT DISTINCT FROM 20 AS is_not_distinct_flag,
    n & 7 AS bit_and_out,
    n | 1 AS bit_or_out,
    n << 1 AS shift_left_out,
    n2 >> 1 AS shift_right_out,
    ~n AS bit_not_out
FROM db9_cop_operator_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/575_pushdown_operator_expressions_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/575_pushdown_operator_expressions_explain.txt; then echo "operator_expression_projection_stays_local|1"; else echo "operator_expression_projection_stays_local|0"; fi
SELECT
    txt LIKE 'A%' AS like_flag,
    txt ILIKE 'a%' AS ilike_flag,
    txt NOT LIKE 'z%' AS not_like_flag,
    n BETWEEN 10 AND 20 AS between_flag,
    n NOT BETWEEN 21 AND 30 AS not_between_flag,
    n IN (10, 20, NULL) AS in_flag,
    n NOT IN (10, NULL) AS not_in_flag,
    flag IS TRUE AS is_true_flag,
    flag IS NOT FALSE AS is_not_false_flag,
    maybe_flag IS UNKNOWN AS is_unknown_flag,
    n IS DISTINCT FROM 10 AS is_distinct_flag,
    n IS NOT DISTINCT FROM 20 AS is_not_distinct_flag,
    n & 7 AS bit_and_out,
    n | 1 AS bit_or_out,
    n << 1 AS shift_left_out,
    n2 >> 1 AS shift_right_out,
    ~n AS bit_not_out
FROM db9_cop_operator_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_operator_on AS
SELECT
    txt LIKE 'A%' AS like_flag,
    txt ILIKE 'a%' AS ilike_flag,
    txt NOT LIKE 'z%' AS not_like_flag,
    n BETWEEN 10 AND 20 AS between_flag,
    n NOT BETWEEN 21 AND 30 AS not_between_flag,
    n IN (10, 20, NULL) AS in_flag,
    n NOT IN (10, NULL) AS not_in_flag,
    flag IS TRUE AS is_true_flag,
    flag IS NOT FALSE AS is_not_false_flag,
    maybe_flag IS UNKNOWN AS is_unknown_flag,
    n IS DISTINCT FROM 10 AS is_distinct_flag,
    n IS NOT DISTINCT FROM 20 AS is_not_distinct_flag,
    n & 7 AS bit_and_out,
    n | 1 AS bit_or_out,
    n << 1 AS shift_left_out,
    n2 >> 1 AS shift_right_out,
    ~n AS bit_not_out
FROM db9_cop_operator_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_operator_off AS
SELECT
    txt LIKE 'A%' AS like_flag,
    txt ILIKE 'a%' AS ilike_flag,
    txt NOT LIKE 'z%' AS not_like_flag,
    n BETWEEN 10 AND 20 AS between_flag,
    n NOT BETWEEN 21 AND 30 AS not_between_flag,
    n IN (10, 20, NULL) AS in_flag,
    n NOT IN (10, NULL) AS not_in_flag,
    flag IS TRUE AS is_true_flag,
    flag IS NOT FALSE AS is_not_false_flag,
    maybe_flag IS UNKNOWN AS is_unknown_flag,
    n IS DISTINCT FROM 10 AS is_distinct_flag,
    n IS NOT DISTINCT FROM 20 AS is_not_distinct_flag,
    n & 7 AS bit_and_out,
    n | 1 AS bit_or_out,
    n << 1 AS shift_left_out,
    n2 >> 1 AS shift_right_out,
    ~n AS bit_not_out
FROM db9_cop_operator_smoke
WHERE n = 20
LIMIT 1;

SELECT 'operator_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_operator_on
            EXCEPT ALL
            SELECT * FROM db9_cop_operator_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_operator_off
            EXCEPT ALL
            SELECT * FROM db9_cop_operator_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_operator_smoke;
DROP TABLE db9_cop_operator_on;
DROP TABLE db9_cop_operator_off;

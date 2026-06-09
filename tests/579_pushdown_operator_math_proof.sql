-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: exact-pair proof for NOT ILIKE, LIKE/ILIKE ESCAPE, ABS, COALESCE, and NULLIF.

DROP TABLE IF EXISTS db9_cop_operator_math_smoke;
DROP TABLE IF EXISTS db9_cop_operator_math_on;
DROP TABLE IF EXISTS db9_cop_operator_math_off;

CREATE TABLE db9_cop_operator_math_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    like_txt TEXT NOT NULL,
    ilike_txt TEXT NOT NULL,
    maybe_txt TEXT,
    fallback_txt TEXT,
    maybe_num INT,
    f8 DOUBLE PRECISION NOT NULL,
    maybe_precision INT,
    cmp_big BIGINT,
    neg_big BIGINT NOT NULL,
    neg_int INT NOT NULL
);
CREATE INDEX db9_cop_operator_math_smoke_n_idx ON db9_cop_operator_math_smoke(n);

INSERT INTO db9_cop_operator_math_smoke VALUES
    (1, 10, 'A_10', 'tmpAlpha', 'present', 'fallback-a', NULL, 12.34, 1, 10, -10, -2),
    (2, 20, 'A_20', 'A%Twenty', NULL, 'Alpha', 20, 12.34, NULL, 99, -20, -5),
    (3, 30, 'AB30', 'tmpThirty', NULL, NULL, NULL, 56.78, 2, NULL, -30, 7);

SET db9.enable_cop_pushdown = on;
\o /tmp/579_operator_math_projection_explain.txt
EXPLAIN SELECT
    ilike_txt NOT ILIKE 'tmp%' AS not_ilike_flag,
    like_txt LIKE 'A!_%' ESCAPE '!' AS like_escape_flag,
    ilike_txt ILIKE 'a!%%' ESCAPE '!' AS ilike_escape_flag,
    ABS(neg_big) AS abs_big_out,
    ABS(neg_int) AS abs_int_out,
    COALESCE(maybe_txt, fallback_txt, 'ultimate') AS coalesce_txt,
    COALESCE(maybe_num, ABS(neg_int), 99) AS coalesce_num,
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out,
    CBRT(f8) AS cbrt_out,
    NULLIF(fallback_txt, maybe_txt) AS nullif_txt,
    NULLIF(ABS(neg_big), cmp_big) AS nullif_num
FROM db9_cop_operator_math_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_operator_math_projection_explain.txt && grep -Fq "DB9 Cop Output: not_ilike_flag, like_escape_flag, ilike_escape_flag, abs_big_out, abs_int_out, coalesce_txt, coalesce_num, round_out, trunc_out, cbrt_out, nullif_txt, nullif_num" /tmp/579_operator_math_projection_explain.txt; then echo "operator_math_projection_folds|1"; else echo "operator_math_projection_folds|0"; fi
SELECT
    ilike_txt NOT ILIKE 'tmp%' AS not_ilike_flag,
    like_txt LIKE 'A!_%' ESCAPE '!' AS like_escape_flag,
    ilike_txt ILIKE 'a!%%' ESCAPE '!' AS ilike_escape_flag,
    ABS(neg_big) AS abs_big_out,
    ABS(neg_int) AS abs_int_out,
    COALESCE(maybe_txt, fallback_txt, 'ultimate') AS coalesce_txt,
    COALESCE(maybe_num, ABS(neg_int), 99) AS coalesce_num,
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out,
    CBRT(f8) AS cbrt_out,
    NULLIF(fallback_txt, maybe_txt) AS nullif_txt,
    NULLIF(ABS(neg_big), cmp_big) AS nullif_num
FROM db9_cop_operator_math_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_operator_math_on AS
SELECT
    n,
    ilike_txt NOT ILIKE 'tmp%' AS not_ilike_flag,
    like_txt LIKE 'A!_%' ESCAPE '!' AS like_escape_flag,
    ilike_txt ILIKE 'a!%%' ESCAPE '!' AS ilike_escape_flag,
    ABS(neg_big) AS abs_big_out,
    ABS(neg_int) AS abs_int_out,
    COALESCE(maybe_txt, fallback_txt, 'ultimate') AS coalesce_txt,
    COALESCE(maybe_num, ABS(neg_int), 99) AS coalesce_num,
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out,
    CBRT(f8) AS cbrt_out,
    NULLIF(fallback_txt, maybe_txt) AS nullif_txt,
    NULLIF(ABS(neg_big), cmp_big) AS nullif_num
FROM db9_cop_operator_math_smoke
ORDER BY n;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_operator_math_off AS
SELECT
    n,
    ilike_txt NOT ILIKE 'tmp%' AS not_ilike_flag,
    like_txt LIKE 'A!_%' ESCAPE '!' AS like_escape_flag,
    ilike_txt ILIKE 'a!%%' ESCAPE '!' AS ilike_escape_flag,
    ABS(neg_big) AS abs_big_out,
    ABS(neg_int) AS abs_int_out,
    COALESCE(maybe_txt, fallback_txt, 'ultimate') AS coalesce_txt,
    COALESCE(maybe_num, ABS(neg_int), 99) AS coalesce_num,
    ROUND(f8) AS round_out,
    TRUNC(f8) AS trunc_out,
    CBRT(f8) AS cbrt_out,
    NULLIF(fallback_txt, maybe_txt) AS nullif_txt,
    NULLIF(ABS(neg_big), cmp_big) AS nullif_num
FROM db9_cop_operator_math_smoke
ORDER BY n;

SELECT 'operator_math_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_operator_math_on
            EXCEPT ALL
            SELECT * FROM db9_cop_operator_math_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_operator_math_off
            EXCEPT ALL
            SELECT * FROM db9_cop_operator_math_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_operator_math_smoke;
DROP TABLE db9_cop_operator_math_on;
DROP TABLE db9_cop_operator_math_off;

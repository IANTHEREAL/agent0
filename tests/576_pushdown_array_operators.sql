-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: unsupported array operators stay local and keep parity.

DROP TABLE IF EXISTS db9_cop_array_operator_smoke;
DROP TABLE IF EXISTS db9_cop_array_operator_on;
DROP TABLE IF EXISTS db9_cop_array_operator_off;

CREATE TABLE db9_cop_array_operator_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    ints INT[] NOT NULL,
    words TEXT[] NOT NULL,
    matrix INT[] NOT NULL
);
CREATE INDEX db9_cop_array_operator_smoke_n_idx ON db9_cop_array_operator_smoke(n);

INSERT INTO db9_cop_array_operator_smoke VALUES
    (1, 10, ARRAY[1, 2, 3], ARRAY['x', 'y'], ARRAY[[1, 2], [3, 4]]),
    (2, 20, ARRAY[1, 2, 3], ARRAY['a', 'b'], ARRAY[[1, 2], [3, 4]]),
    (3, 30, ARRAY[4, 5, 6], ARRAY['c', 'd'], ARRAY[[5, 6], [7, 8]]);

SET db9.enable_cop_pushdown = on;
\o /tmp/576_pushdown_array_operators_explain.txt
EXPLAIN SELECT
    ints @> ARRAY[2] AS contains_scalar_flag,
    ARRAY[2] <@ ints AS contained_scalar_flag,
    ints && ARRAY[2, 9] AS overlap_scalar_flag,
    matrix @> ARRAY[2, 3] AS contains_flatten_flag,
    ARRAY[2, 3] <@ matrix AS contained_flatten_flag,
    matrix && ARRAY[2, 9] AS overlap_flatten_flag,
    words @> ARRAY['a'] AS text_contains_flag,
    words && ARRAY['z', 'a'] AS text_overlap_flag
FROM db9_cop_array_operator_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/576_pushdown_array_operators_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/576_pushdown_array_operators_explain.txt; then echo "array_operator_projection_stays_local|1"; else echo "array_operator_projection_stays_local|0"; fi
SELECT
    ints @> ARRAY[2] AS contains_scalar_flag,
    ARRAY[2] <@ ints AS contained_scalar_flag,
    ints && ARRAY[2, 9] AS overlap_scalar_flag,
    matrix @> ARRAY[2, 3] AS contains_flatten_flag,
    ARRAY[2, 3] <@ matrix AS contained_flatten_flag,
    matrix && ARRAY[2, 9] AS overlap_flatten_flag,
    words @> ARRAY['a'] AS text_contains_flag,
    words && ARRAY['z', 'a'] AS text_overlap_flag
FROM db9_cop_array_operator_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_array_operator_on AS
SELECT
    ints @> ARRAY[2] AS contains_scalar_flag,
    ARRAY[2] <@ ints AS contained_scalar_flag,
    ints && ARRAY[2, 9] AS overlap_scalar_flag,
    matrix @> ARRAY[2, 3] AS contains_flatten_flag,
    ARRAY[2, 3] <@ matrix AS contained_flatten_flag,
    matrix && ARRAY[2, 9] AS overlap_flatten_flag,
    words @> ARRAY['a'] AS text_contains_flag,
    words && ARRAY['z', 'a'] AS text_overlap_flag
FROM db9_cop_array_operator_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_array_operator_off AS
SELECT
    ints @> ARRAY[2] AS contains_scalar_flag,
    ARRAY[2] <@ ints AS contained_scalar_flag,
    ints && ARRAY[2, 9] AS overlap_scalar_flag,
    matrix @> ARRAY[2, 3] AS contains_flatten_flag,
    ARRAY[2, 3] <@ matrix AS contained_flatten_flag,
    matrix && ARRAY[2, 9] AS overlap_flatten_flag,
    words @> ARRAY['a'] AS text_contains_flag,
    words && ARRAY['z', 'a'] AS text_overlap_flag
FROM db9_cop_array_operator_smoke
WHERE n = 20
LIMIT 1;

SELECT 'array_operator_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_array_operator_on
            EXCEPT ALL
            SELECT * FROM db9_cop_array_operator_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_array_operator_off
            EXCEPT ALL
            SELECT * FROM db9_cop_array_operator_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_array_operator_smoke;
DROP TABLE db9_cop_array_operator_on;
DROP TABLE db9_cop_array_operator_off;

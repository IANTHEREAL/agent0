-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: array builtin semantics stay PG-compatible under pushdown.

DROP TABLE IF EXISTS db9_cop_array_function_smoke;
DROP TABLE IF EXISTS db9_cop_array_function_on;
DROP TABLE IF EXISTS db9_cop_array_function_off;

CREATE TABLE db9_cop_array_function_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL
);
CREATE INDEX db9_cop_array_function_smoke_n_idx ON db9_cop_array_function_smoke(n);

INSERT INTO db9_cop_array_function_smoke VALUES
    (1, 10),
    (2, 20),
    (3, 30);

SET db9.enable_cop_pushdown = on;
\! rm -f /tmp/578_pushdown_array_function_explain.txt
\o /tmp/578_pushdown_array_function_explain.txt
EXPLAIN SELECT
    array_cat(NULL::INT[], NULL::INT[]) IS NULL AS array_cat_null_is_null,
    array_cat(NULL::INT[], ARRAY[1, 2]) = ARRAY[1, 2] AS array_cat_null_left_is_noop,
    array_cat(ARRAY[1, 2], NULL::INT[]) = ARRAY[1, 2] AS array_cat_null_right_is_noop,
    array_to_string(ARRAY['a', NULL, 'b'], NULL) IS NULL AS array_to_string_null_delim_is_null,
    string_to_array('abc', '') = ARRAY['abc'] AS string_to_array_empty_delim_keeps_whole_input,
    string_to_array('', ',') = ARRAY[]::TEXT[] AS string_to_array_empty_input_is_empty
FROM db9_cop_array_function_smoke
WHERE n = 20
LIMIT 1;
\o
\! cat /tmp/578_pushdown_array_function_explain.txt
\! if grep -Fq "DB9 Cop Output:" /tmp/578_pushdown_array_function_explain.txt; then echo "array_function_projection_stays_local|0"; else echo "array_function_projection_stays_local|1"; fi
SELECT
    array_cat(NULL::INT[], NULL::INT[]) IS NULL AS array_cat_null_is_null,
    array_cat(NULL::INT[], ARRAY[1, 2]) = ARRAY[1, 2] AS array_cat_null_left_is_noop,
    array_cat(ARRAY[1, 2], NULL::INT[]) = ARRAY[1, 2] AS array_cat_null_right_is_noop,
    array_to_string(ARRAY['a', NULL, 'b'], NULL) IS NULL AS array_to_string_null_delim_is_null,
    string_to_array('abc', '') = ARRAY['abc'] AS string_to_array_empty_delim_keeps_whole_input,
    string_to_array('', ',') = ARRAY[]::TEXT[] AS string_to_array_empty_input_is_empty
FROM db9_cop_array_function_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_array_function_on AS
SELECT
    array_cat(NULL::INT[], NULL::INT[]) IS NULL AS array_cat_null_is_null,
    array_cat(NULL::INT[], ARRAY[1, 2]) = ARRAY[1, 2] AS array_cat_null_left_is_noop,
    array_cat(ARRAY[1, 2], NULL::INT[]) = ARRAY[1, 2] AS array_cat_null_right_is_noop,
    array_to_string(ARRAY['a', NULL, 'b'], NULL) IS NULL AS array_to_string_null_delim_is_null,
    string_to_array('abc', '') = ARRAY['abc'] AS string_to_array_empty_delim_keeps_whole_input,
    string_to_array('', ',') = ARRAY[]::TEXT[] AS string_to_array_empty_input_is_empty
FROM db9_cop_array_function_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_array_function_off AS
SELECT
    array_cat(NULL::INT[], NULL::INT[]) IS NULL AS array_cat_null_is_null,
    array_cat(NULL::INT[], ARRAY[1, 2]) = ARRAY[1, 2] AS array_cat_null_left_is_noop,
    array_cat(ARRAY[1, 2], NULL::INT[]) = ARRAY[1, 2] AS array_cat_null_right_is_noop,
    array_to_string(ARRAY['a', NULL, 'b'], NULL) IS NULL AS array_to_string_null_delim_is_null,
    string_to_array('abc', '') = ARRAY['abc'] AS string_to_array_empty_delim_keeps_whole_input,
    string_to_array('', ',') = ARRAY[]::TEXT[] AS string_to_array_empty_input_is_empty
FROM db9_cop_array_function_smoke
WHERE n = 20
LIMIT 1;

SELECT 'array_function_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_array_function_on
            EXCEPT ALL
            SELECT * FROM db9_cop_array_function_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_array_function_off
            EXCEPT ALL
            SELECT * FROM db9_cop_array_function_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE db9_cop_array_function_smoke;
DROP TABLE db9_cop_array_function_on;
DROP TABLE db9_cop_array_function_off;

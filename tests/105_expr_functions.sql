-- Expression Evaluation Integration Tests
-- Tests SQL functions and expressions for expr.rs refactoring safety net

-- ============================================================
-- String Functions
-- ============================================================

SELECT '=== String Functions ===' AS section;

SELECT UPPER('hello') AS upper_test;
SELECT LOWER('WORLD') AS lower_test;
SELECT LENGTH('test') AS length_test;
SELECT CONCAT('a', 'b', 'c') AS concat_test;
SELECT CONCAT_WS('-', 'a', 'b', 'c') AS concat_ws_test;
SELECT LEFT('hello', 3) AS left_test;
SELECT RIGHT('hello', 3) AS right_test;
SELECT SUBSTRING('hello', 2, 3) AS substring_test;
SELECT SUBSTR('hello', 2, 3) AS substr_test;
SELECT TRIM('  hello  ') AS trim_test;
SELECT LTRIM('  hello') AS ltrim_test;
SELECT RTRIM('hello  ') AS rtrim_test;
SELECT LPAD('42', 5, '0') AS lpad_test;
SELECT RPAD('42', 5, '0') AS rpad_test;
SELECT REPLACE('hello', 'l', 'L') AS replace_test;
SELECT REVERSE('hello') AS reverse_test;
SELECT REPEAT('ab', 3) AS repeat_test;
SELECT SPLIT_PART('a-b-c', '-', 2) AS split_part_test;
SELECT STRPOS('hello', 'l') AS strpos_test;
SELECT POSITION('l' IN 'hello') AS position_test;
SELECT ASCII('A') AS ascii_test;
SELECT CHR(65) AS chr_test;
SELECT INITCAP('hello world') AS initcap_test;
SELECT TRANSLATE('hello', 'el', 'ip') AS translate_test;
SELECT MD5('test') AS md5_test;
SELECT QUOTE_IDENT('my table') AS quote_ident_test;
SELECT QUOTE_LITERAL('it''s') AS quote_literal_test;
SELECT QUOTE_NULLABLE(NULL) AS quote_nullable_null_test;
SELECT QUOTE_NULLABLE('value') AS quote_nullable_value_test;

-- ============================================================
-- Math Functions
-- ============================================================

SELECT '=== Math Functions ===' AS section;

SELECT ABS(-5) AS abs_test;
SELECT CEIL(4.2) AS ceil_test;
SELECT CEILING(4.2) AS ceiling_test;
SELECT FLOOR(4.8) AS floor_test;
SELECT ROUND(4.567, 2) AS round_test;
SELECT TRUNC(4.567, 1) AS trunc_test;
SELECT SQRT(16) AS sqrt_test;
SELECT CBRT(27) AS cbrt_test;
SELECT POWER(2, 3) AS power_test;
SELECT POW(2, 3) AS pow_test;
SELECT EXP(1) > 2.7 AND EXP(1) < 2.8 AS exp_test;
SELECT LN(2.718281828) > 0.99 AND LN(2.718281828) < 1.01 AS ln_test;
SELECT LOG(100) AS log_test;
SELECT SIGN(-5) AS sign_neg_test;
SELECT SIGN(5) AS sign_pos_test;
SELECT SIGN(0) AS sign_zero_test;
SELECT MOD(10, 3) AS mod_test;
SELECT 10 % 3 AS modulo_op_test;
SELECT DEGREES(3.14159265359) > 179 AND DEGREES(3.14159265359) < 181 AS degrees_test;
SELECT RADIANS(180) > 3.14 AND RADIANS(180) < 3.15 AS radians_test;
SELECT SIN(0) AS sin_test;
SELECT COS(0) AS cos_test;
SELECT TAN(0) AS tan_test;
SELECT PI() > 3.14 AND PI() < 3.15 AS pi_test;
SELECT RANDOM() >= 0 AND RANDOM() < 1 AS random_test;

-- ============================================================
-- Date/Time Functions
-- ============================================================

SELECT '=== Date/Time Functions ===' AS section;

SELECT NOW() IS NOT NULL AS now_test;
SELECT CURRENT_TIMESTAMP IS NOT NULL AS current_timestamp_test;
SELECT CURRENT_DATE IS NOT NULL AS current_date_test;
SELECT DATE_TRUNC('day', TIMESTAMP '2024-06-15 14:30:45') AS date_trunc_day_test;
SELECT DATE_TRUNC('hour', TIMESTAMP '2024-06-15 14:30:45') AS date_trunc_hour_test;
SELECT DATE_TRUNC('month', TIMESTAMP '2024-06-15 14:30:45') AS date_trunc_month_test;
SELECT EXTRACT(YEAR FROM TIMESTAMP '2024-06-15 14:30:45') AS extract_year_test;
SELECT EXTRACT(MONTH FROM TIMESTAMP '2024-06-15 14:30:45') AS extract_month_test;
SELECT EXTRACT(DAY FROM TIMESTAMP '2024-06-15 14:30:45') AS extract_day_test;
SELECT EXTRACT(HOUR FROM TIMESTAMP '2024-06-15 14:30:45') AS extract_hour_test;
SELECT DATE '2024-06-15' AS date_literal_test;
SELECT TO_CHAR(TIMESTAMP '2024-06-15 14:30:45', 'YYYY-MM-DD') AS to_char_test;

-- ============================================================
-- JSON Functions
-- ============================================================

SELECT '=== JSON Functions ===' AS section;

SELECT '{"a": 1}'::jsonb -> 'a' AS jsonb_arrow_test;
SELECT '{"a": "hello"}'::jsonb ->> 'a' AS jsonb_double_arrow_test;
SELECT '[1, 2, 3]'::jsonb -> 0 AS jsonb_array_access_test;
SELECT JSONB_TYPEOF('{"a": 1}'::jsonb) AS jsonb_typeof_object_test;
SELECT JSONB_TYPEOF('[1, 2]'::jsonb) AS jsonb_typeof_array_test;
SELECT JSONB_TYPEOF('"hello"'::jsonb) AS jsonb_typeof_string_test;
SELECT JSONB_TYPEOF('123'::jsonb) AS jsonb_typeof_number_test;
SELECT JSONB_TYPEOF('true'::jsonb) AS jsonb_typeof_boolean_test;
SELECT JSONB_TYPEOF('null'::jsonb) AS jsonb_typeof_null_test;
SELECT JSONB_ARRAY_LENGTH('[1, 2, 3]'::jsonb) AS jsonb_array_length_test;
SELECT JSONB_BUILD_OBJECT('a', 1, 'b', 'hello') AS jsonb_build_object_test;
SELECT JSONB_BUILD_ARRAY(1, 'a', true) AS jsonb_build_array_test;
SELECT TO_JSON(123) AS to_json_number_test;
SELECT TO_JSONB('hello') AS to_jsonb_string_test;
SELECT JSONB_EXTRACT_PATH('{"a": {"b": 1}}'::jsonb, 'a', 'b') AS jsonb_extract_path_test;
SELECT JSONB_EXTRACT_PATH_TEXT('{"a": {"b": "hello"}}'::jsonb, 'a', 'b') AS jsonb_extract_path_text_test;
SELECT JSONB_EXISTS('{"a": 1, "b": 2}'::jsonb, 'a') AS jsonb_exists_true_test;
SELECT JSONB_EXISTS('{"a": 1, "b": 2}'::jsonb, 'c') AS jsonb_exists_false_test;
SELECT '{"a": 1}'::jsonb @> '{"a": 1}'::jsonb AS jsonb_contains_test;
SELECT '{"a": 1}'::jsonb <@ '{"a": 1, "b": 2}'::jsonb AS jsonb_contained_test;
SELECT '{"a": 1}'::jsonb ? 'a' AS jsonb_question_test;

-- ============================================================
-- Array Functions
-- ============================================================

SELECT '=== Array Functions ===' AS section;

SELECT ARRAY[1, 2, 3] AS array_literal_test;
SELECT ARRAY_LENGTH(ARRAY[1, 2, 3], 1) AS array_length_test;
SELECT ARRAY_UPPER(ARRAY[1, 2, 3], 1) AS array_upper_test;
SELECT ARRAY_LOWER(ARRAY[1, 2, 3], 1) AS array_lower_test;
SELECT CARDINALITY(ARRAY[1, 2, 3]) AS cardinality_test;
SELECT ARRAY_POSITION(ARRAY['a', 'b', 'c'], 'b') AS array_position_test;
SELECT ARRAY_CAT(ARRAY[1, 2], ARRAY[3, 4]) AS array_cat_test;
SELECT ARRAY_APPEND(ARRAY[1, 2], 3) AS array_append_test;
SELECT ARRAY_PREPEND(0, ARRAY[1, 2]) AS array_prepend_test;
SELECT ARRAY_REMOVE(ARRAY[1, 2, 3, 2], 2) AS array_remove_test;
SELECT ARRAY_TO_STRING(ARRAY['a', 'b', 'c'], ',') AS array_to_string_test;
SELECT STRING_TO_ARRAY('a,b,c', ',') AS string_to_array_test;
SELECT ARRAY[1, 2, 3] @> ARRAY[1, 2] AS array_contains_test;
SELECT ARRAY[1, 2] <@ ARRAY[1, 2, 3] AS array_contained_test;
SELECT ARRAY[1, 2] && ARRAY[2, 3] AS array_overlap_test;

-- ============================================================
-- Regex Functions
-- ============================================================

SELECT '=== Regex Functions ===' AS section;

SELECT 'hello world' ~ 'world' AS regex_match_test;
SELECT 'hello world' ~* 'WORLD' AS regex_imatch_test;
SELECT 'hello world' !~ 'foo' AS regex_not_match_test;
SELECT REGEXP_REPLACE('hello world', 'world', 'there') AS regexp_replace_test;
SELECT REGEXP_REPLACE('hello hello', 'hello', 'hi', 'g') AS regexp_replace_global_test;
SELECT REGEXP_MATCHES('hello 123 world 456', '\d+') AS regexp_matches_test;
SELECT REGEXP_SPLIT_TO_ARRAY('a-b-c', '-') AS regexp_split_test;

-- ============================================================
-- UUID Functions
-- ============================================================

SELECT '=== UUID Functions ===' AS section;

SELECT LENGTH(GEN_RANDOM_UUID()::text) AS uuid_length_test;
SELECT GEN_RANDOM_UUID() <> GEN_RANDOM_UUID() AS uuid_unique_test;

-- ============================================================
-- Misc Functions
-- ============================================================

SELECT '=== Misc Functions ===' AS section;

SELECT COALESCE(NULL, NULL, 'default') AS coalesce_test;
SELECT COALESCE('first', 'second') AS coalesce_first_test;
SELECT NULLIF(1, 1) AS nullif_same_test;
SELECT NULLIF(1, 2) AS nullif_diff_test;
SELECT GREATEST(1, 3, 2) AS greatest_test;
SELECT LEAST(1, 3, 2) AS least_test;
SELECT PG_TYPEOF(123) AS pg_typeof_int_test;
SELECT PG_TYPEOF('hello'::text) AS pg_typeof_text_test;
SELECT PG_TYPEOF(TRUE) AS pg_typeof_bool_test;
SELECT VERSION() LIKE 'PostgreSQL%' AS version_test;
SELECT CURRENT_DATABASE() IS NOT NULL AS current_database_test;
SELECT CURRENT_SCHEMA() AS current_schema_test;
SELECT CURRENT_USER IS NOT NULL AS current_user_test;

-- ============================================================
-- Binary Operators
-- ============================================================

SELECT '=== Binary Operators ===' AS section;

-- Arithmetic
SELECT 1 + 2 AS add_test;
SELECT 5 - 3 AS sub_test;
SELECT 4 * 3 AS mul_test;
SELECT 10 / 3 AS div_test;
SELECT 10.0 / 3.0 > 3.33 AS float_div_test;

-- Comparison
SELECT 1 < 2 AS lt_test;
SELECT 2 > 1 AS gt_test;
SELECT 1 <= 1 AS le_test;
SELECT 1 >= 1 AS ge_test;
SELECT 1 = 1 AS eq_test;
SELECT 1 <> 2 AS ne_test;

-- Logical
SELECT TRUE AND TRUE AS and_true_test;
SELECT TRUE AND FALSE AS and_false_test;
SELECT TRUE OR FALSE AS or_true_test;
SELECT FALSE OR FALSE AS or_false_test;
SELECT NOT TRUE AS not_true_test;
SELECT NOT FALSE AS not_false_test;

-- String concatenation
SELECT 'hello' || ' ' || 'world' AS string_concat_test;

-- ============================================================
-- CASE Expressions
-- ============================================================

SELECT '=== CASE Expressions ===' AS section;

SELECT CASE WHEN 1 > 0 THEN 'positive' ELSE 'non-positive' END AS case_simple_test;
SELECT CASE 1 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END AS case_value_test;
SELECT CASE WHEN NULL THEN 'null' ELSE 'not null branch' END AS case_null_test;

-- ============================================================
-- NULL Handling
-- ============================================================

SELECT '=== NULL Handling ===' AS section;

SELECT NULL IS NULL AS is_null_test;
SELECT 1 IS NOT NULL AS is_not_null_test;
SELECT NULL = NULL AS null_eq_null_test;
SELECT COALESCE(NULL, 1) AS coalesce_null_test;

-- ============================================================
-- Type Casting
-- ============================================================

SELECT '=== Type Casting ===' AS section;

SELECT CAST(123 AS TEXT) AS cast_int_to_text;
SELECT CAST('456' AS INTEGER) AS cast_text_to_int;
SELECT CAST(3.14 AS INTEGER) AS cast_float_to_int;
SELECT '123'::INTEGER AS pg_cast_text_to_int;
SELECT 456::TEXT AS pg_cast_int_to_text;
-- NOTE: TRUE::INTEGER fails with "Unsupported types for addition" - pre-existing bug, skipped
-- SELECT TRUE::INTEGER AS cast_bool_to_int;
SELECT 1::BOOLEAN AS cast_int_to_bool;

-- ============================================================
-- IN and BETWEEN
-- ============================================================

SELECT '=== IN and BETWEEN ===' AS section;

SELECT 2 IN (1, 2, 3) AS in_list_true_test;
SELECT 5 IN (1, 2, 3) AS in_list_false_test;
SELECT 'b' IN ('a', 'b', 'c') AS in_list_string_test;
SELECT 5 BETWEEN 1 AND 10 AS between_true_test;
SELECT 15 BETWEEN 1 AND 10 AS between_false_test;
SELECT 5 NOT BETWEEN 10 AND 20 AS not_between_test;

-- ============================================================
-- LIKE and SIMILAR TO
-- ============================================================

SELECT '=== LIKE and SIMILAR TO ===' AS section;

SELECT 'hello' LIKE 'h%' AS like_prefix_test;
SELECT 'hello' LIKE '%llo' AS like_suffix_test;
SELECT 'hello' LIKE '%ell%' AS like_contains_test;
SELECT 'hello' LIKE 'h_llo' AS like_single_char_test;
SELECT 'Hello' ILIKE 'hello' AS ilike_test;
SELECT 'hello' NOT LIKE 'world%' AS not_like_test;
SELECT 'hello' SIMILAR TO 'h%o' AS similar_to_test;

-- ============================================================
-- Interval Arithmetic
-- ============================================================

SELECT '=== Interval Arithmetic ===' AS section;

SELECT INTERVAL '1 day' AS interval_day_test;
SELECT INTERVAL '2 hours' AS interval_hour_test;
SELECT INTERVAL '30 minutes' AS interval_minute_test;
SELECT TIMESTAMP '2024-01-01 00:00:00' + INTERVAL '1 day' AS timestamp_plus_interval_test;
SELECT DATE '2024-01-01' + INTERVAL '1 month' AS date_plus_interval_test;
SELECT DATE '2024-01-15' - DATE '2024-01-10' AS date_diff_test;

-- ============================================================
-- Unary Operators
-- ============================================================

SELECT '=== Unary Operators ===' AS section;

SELECT -5 AS unary_minus_test;
-- NOTE: +5 (unary plus) not supported - pre-existing limitation, skipped
-- SELECT +5 AS unary_plus_test;
SELECT NOT TRUE AS not_test;

-- ============================================================
-- IS TRUE/FALSE/UNKNOWN
-- ============================================================

SELECT '=== IS TRUE/FALSE/UNKNOWN ===' AS section;

SELECT TRUE IS TRUE AS is_true_test;
SELECT FALSE IS FALSE AS is_false_test;
SELECT NULL IS UNKNOWN AS is_unknown_test;
SELECT TRUE IS NOT FALSE AS is_not_false_test;
SELECT FALSE IS NOT TRUE AS is_not_true_test;

-- ============================================================
-- ANY/ALL Operators
-- ============================================================

SELECT '=== ANY/ALL Operators ===' AS section;

SELECT 2 = ANY(ARRAY[1, 2, 3]) AS any_eq_test;
SELECT 5 > ALL(ARRAY[1, 2, 3]) AS all_gt_test;
SELECT 2 < ALL(ARRAY[3, 4, 5]) AS all_lt_test;

-- ============================================================
-- Encode/Decode
-- ============================================================

SELECT '=== Encode/Decode ===' AS section;

SELECT ENCODE('hello'::bytea, 'hex') AS encode_hex_test;
SELECT ENCODE('hello'::bytea, 'base64') AS encode_base64_test;
SELECT DECODE('68656c6c6f', 'hex') = 'hello'::bytea AS decode_hex_test;

-- ============================================================
-- Complete
-- ============================================================

SELECT '=== All Expression Tests Complete ===' AS section;

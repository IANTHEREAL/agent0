SELECT 'hello' || ' ' || 'world' AS concat_op;
SELECT CONCAT('a', 'b', 'c', 'd') AS concat_func;
SELECT CONCAT_WS('-', 'a', 'b', 'c') AS concat_ws;
SELECT CONCAT_WS(',', 'a', NULL, 'b', NULL, 'c') AS concat_ws_nulls;

SELECT LENGTH('hello') AS len;
SELECT CHAR_LENGTH('hello') AS char_len;
SELECT CHARACTER_LENGTH('hello') AS character_len;
SELECT OCTET_LENGTH('hello') AS octet_len;
SELECT BIT_LENGTH('hello') AS bit_len;

SELECT UPPER('Hello World') AS upper_val;
SELECT LOWER('Hello World') AS lower_val;
SELECT INITCAP('hello world') AS initcap_val;

SELECT TRIM('  hello  ') AS trim_both;
SELECT LTRIM('  hello  ') AS ltrim_val;
SELECT RTRIM('  hello  ') AS rtrim_val;
SELECT TRIM(LEADING 'x' FROM 'xxxhello') AS trim_leading;
SELECT TRIM(TRAILING 'o' FROM 'hellooo') AS trim_trailing;
SELECT TRIM(BOTH 'x' FROM 'xxhelloxx') AS trim_both_char;
SELECT BTRIM('xxhelloxx', 'x') AS btrim_val;

SELECT LEFT('hello world', 5) AS left_val;
SELECT RIGHT('hello world', 5) AS right_val;
SELECT SUBSTRING('hello world' FROM 7 FOR 5) AS substr_from_for;
SELECT SUBSTRING('hello world' FROM 7) AS substr_from;
SELECT SUBSTR('hello world', 7, 5) AS substr_func;

SELECT POSITION('world' IN 'hello world') AS pos;
SELECT STRPOS('hello world', 'world') AS strpos;

SELECT REPLACE('hello world', 'world', 'there') AS replace_val;
SELECT TRANSLATE('hello', 'el', 'ip') AS translate_val;
SELECT OVERLAY('hello' PLACING 'XX' FROM 3 FOR 2) AS overlay_val;

SELECT REPEAT('ab', 3) AS repeat_val;
SELECT REVERSE('hello') AS reverse_val;

SELECT LPAD('hello', 10, '*') AS lpad_val;
SELECT RPAD('hello', 10, '*') AS rpad_val;
SELECT LPAD('hello', 3) AS lpad_trunc;

SELECT SPLIT_PART('a,b,c,d', ',', 2) AS split_part;
SELECT SPLIT_PART('a,b,c,d', ',', 5) AS split_part_empty;

SELECT ASCII('A') AS ascii_val;
SELECT CHR(65) AS chr_val;

SELECT MD5('hello') AS md5_hash;

SELECT ENCODE('hello'::BYTEA, 'base64') AS base64;
SELECT DECODE('aGVsbG8=', 'base64') AS decoded;
SELECT ENCODE('hello'::BYTEA, 'hex') AS hex_val;

SELECT FORMAT('%s %s', 'hello', 'world') AS format_s;
SELECT FORMAT('%I', 'column name') AS format_i;
SELECT FORMAT('%L', 'it''s a test') AS format_l;

SELECT QUOTE_IDENT('hello') AS normal_ident;
SELECT QUOTE_IDENT('Hello World') AS needs_quotes;
SELECT QUOTE_LITERAL('hello') AS normal_lit;
SELECT QUOTE_LITERAL('it''s') AS escaped_lit;

SELECT 'hello' LIKE 'h%' AS like_prefix;
SELECT 'hello' LIKE '%llo' AS like_suffix;
SELECT 'hello' LIKE '%ell%' AS like_contains;
SELECT 'hello' LIKE 'h_llo' AS like_single;
SELECT 'hello' LIKE 'H%' AS like_case;
SELECT 'hello' ILIKE 'H%' AS ilike_case;

SELECT 'PostgreSQL tests completed' AS result;

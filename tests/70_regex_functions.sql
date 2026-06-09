-- PG_PARITY: PostgreSQL-compatible regular expression behavior.
-- Regular Expression Functions Tests

SELECT 'hello world' ~ 'hello' AS matches;
SELECT 'hello world' ~ 'HELLO' AS case_sensitive_no_match;
SELECT 'hello world' ~* 'HELLO' AS case_insensitive_match;
SELECT 'hello world' !~ 'goodbye' AS not_matches;
SELECT 'hello world' !~* 'GOODBYE' AS not_matches_insensitive;

SELECT 'abc123def456' ~ '^[a-z]+[0-9]+' AS starts_with_letters_then_numbers;
SELECT 'hello world' ~ '\s' AS contains_whitespace;
SELECT 'test@example.com' ~ '^[^@]+@[^@]+\.[^@]+$' AS email_pattern;

SELECT REGEXP_REPLACE('hello world', 'world', 'there') AS basic_replace;
SELECT REGEXP_REPLACE('foo bar foo', 'foo', 'baz') AS replace_first;
SELECT REGEXP_REPLACE('foo bar foo', 'foo', 'baz', 'g') AS replace_all;
SELECT REGEXP_REPLACE('Hello World', '[aeiou]', '*', 'gi') AS vowels_replaced;
SELECT REGEXP_REPLACE('abc123def456', '[0-9]+', 'NUM', 'g') AS numbers_replaced;
SELECT REPLACE(REGEXP_REPLACE('a' || CHR(10) || 'b', '.', 'X', 'gp'), CHR(10), '<NL>') AS replace_p_partial_newline;
SELECT REPLACE(REGEXP_REPLACE('a' || CHR(10) || 'b', '.', 'X', 'gw'), CHR(10), '<NL>') AS replace_w_inverse_partial_newline;
SELECT REPLACE(REGEXP_REPLACE('a' || CHR(10) || 'b', '^b$', 'X', 'gp'), CHR(10), '<NL>') AS replace_p_anchor_mode;
SELECT REPLACE(REGEXP_REPLACE('a' || CHR(10) || 'b', '^b$', 'X', 'gw'), CHR(10), '<NL>') AS replace_w_anchor_mode;

SELECT REGEXP_MATCHES('abc 123 def 456', '[0-9]+') AS first_match;
SELECT REGEXP_MATCHES('abc 123 def 456', '[0-9]+', 'g') AS all_matches;
SELECT REGEXP_MATCHES('foo bar baz', '(\w+)', 'g') AS all_words;

SELECT SUBSTRING('hello world' FROM 'w\w+') AS extract_word_starting_w;
SELECT SUBSTRING('test123' FROM '[0-9]+') AS extract_numbers;
SELECT SUBSTRING('abc@example.com' FROM '@(.+)$') AS extract_domain;

SELECT REGEXP_SPLIT_TO_TABLE('one,two,three', ',') AS split_result;
SELECT REGEXP_SPLIT_TO_TABLE('a1b2c3', '[0-9]') AS split_by_numbers;

SELECT REGEXP_SPLIT_TO_ARRAY('one,two,three', ',') AS split_array;
SELECT REGEXP_SPLIT_TO_ARRAY('hello   world', '\s+') AS split_whitespace;

DROP TABLE IF EXISTS log_entries CASCADE;
CREATE TABLE log_entries (
    id INT PRIMARY KEY,
    log_line TEXT
);

INSERT INTO log_entries VALUES
    (1, 'ERROR: Connection failed at 10.0.0.1:5432'),
    (2, 'INFO: User admin logged in'),
    (3, 'WARN: High memory usage: 85%'),
    (4, 'ERROR: Timeout after 30s');

SELECT id, log_line FROM log_entries WHERE log_line ~ '^ERROR' ORDER BY id;
SELECT id, REGEXP_REPLACE(log_line, '^[A-Z]+: ', '') AS message FROM log_entries ORDER BY id;

DROP TABLE log_entries;

SELECT 'Regular expression tests completed' AS result;

-- Regression test for #2299: parser must not panic on multibyte UTF-8 text.
-- Before the fix, the rewrite tokenizer advanced byte-by-byte on non-ASCII
-- characters and sliced at non-char-boundary positions, causing a panic.
-- After the fix, multibyte chars are consumed as identifier tokens (matching
-- PostgreSQL behavior) and invalid SQL returns a normal parse error.

-- 1. Chinese text in string literals
SELECT '中文测试' AS label;

-- 2. Chinese in double-quoted identifiers
SELECT 1 AS "列名";

-- 3. Mixed multibyte: 2-byte (é), 3-byte (中), 4-byte (🎉)
SELECT 'café' AS two_byte, '中文' AS three_byte, '🎉' AS four_byte;

-- 4. Multibyte in comments — must not panic
-- 这是一个注释
SELECT 1 AS comment_test;

/* 块注释：コメント */
SELECT 2 AS block_comment_test;

-- 5. Multibyte in dollar-quoted string
SELECT $$日本語テスト$$ AS dollar_test;

-- 6. Table with multibyte column data
CREATE TABLE utf8_test_2299 (id INT, name TEXT);
INSERT INTO utf8_test_2299 VALUES (1, '太郎');
INSERT INTO utf8_test_2299 VALUES (2, '花子');
SELECT id, name FROM utf8_test_2299 ORDER BY id;

-- 7. jsonb ? operator with Chinese in string (triggers rewrite path)
SELECT '{"模型": "value"}'::jsonb ? '模型';

-- 8. Multibyte text in ORDER BY
SELECT name FROM utf8_test_2299 ORDER BY name;

-- Cleanup
DROP TABLE utf8_test_2299;

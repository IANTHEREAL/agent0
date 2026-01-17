DROP TABLE IF EXISTS str_test CASCADE;

CREATE TABLE str_test (
    id INT PRIMARY KEY,
    s TEXT
);

INSERT INTO str_test VALUES (1, 'Hello World');
INSERT INTO str_test VALUES (2, '  trimmed  ');
INSERT INTO str_test VALUES (3, 'UPPERCASE');
INSERT INTO str_test VALUES (4, 'lowercase');

SELECT id, LENGTH(s) AS len FROM str_test ORDER BY id;
SELECT id, UPPER(s) AS upper_s FROM str_test ORDER BY id;
SELECT id, LOWER(s) AS lower_s FROM str_test ORDER BY id;
SELECT id, INITCAP(s) AS initcap_s FROM str_test ORDER BY id;

SELECT id, TRIM(s) AS trimmed FROM str_test WHERE id = 2;
SELECT id, LTRIM(s) AS ltrimmed FROM str_test WHERE id = 2;
SELECT id, RTRIM(s) AS rtrimmed FROM str_test WHERE id = 2;
SELECT TRIM(BOTH 'x' FROM 'xxxhelloxxxx') AS trim_char;

SELECT id, LEFT(s, 5) AS left5 FROM str_test ORDER BY id;
SELECT id, RIGHT(s, 5) AS right5 FROM str_test ORDER BY id;
SELECT id, SUBSTRING(s, 1, 5) AS substr FROM str_test ORDER BY id;
SELECT id, SUBSTRING(s FROM 7) AS substr_from FROM str_test WHERE id = 1;

SELECT CONCAT('Hello', ' ', 'World') AS concatenated;
SELECT CONCAT_WS('-', 'a', 'b', 'c') AS concat_ws;
SELECT 'Hello' || ' ' || 'World' AS pipe_concat;

SELECT REPLACE('Hello World', 'World', 'PostgreSQL') AS replaced;
SELECT REPEAT('ab', 3) AS repeated;
SELECT REVERSE('Hello') AS reversed;

SELECT POSITION('World' IN 'Hello World') AS pos;
SELECT STRPOS('Hello World', 'World') AS strpos;

SELECT LPAD('42', 5, '0') AS lpadded;
SELECT RPAD('42', 5, '0') AS rpadded;

SELECT SPLIT_PART('a,b,c', ',', 2) AS second_part;

SELECT id, s FROM str_test WHERE s LIKE '%World%' ORDER BY id;
SELECT id, s FROM str_test WHERE s LIKE 'Hello%' ORDER BY id;
SELECT id, s FROM str_test WHERE s ILIKE '%case%' ORDER BY id;

SELECT ASCII('A') AS ascii_a;
SELECT CHR(65) AS chr_65;
SELECT FORMAT('Hello %s', 'World') AS formatted;
SELECT TRANSLATE('hello', 'el', 'ip') AS translated;

DROP TABLE str_test;

SELECT 'String function tests completed' AS result;

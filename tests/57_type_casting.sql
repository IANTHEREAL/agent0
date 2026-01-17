DROP TABLE IF EXISTS cast_test CASCADE;

CREATE TABLE cast_test (
    id INT PRIMARY KEY,
    int_val INT,
    float_val DOUBLE PRECISION,
    text_val TEXT
);

INSERT INTO cast_test VALUES (1, 42, 3.14159, 'hello');
INSERT INTO cast_test VALUES (2, -100, -2.71828, '12345');
INSERT INTO cast_test VALUES (3, 0, 0.0, 'true');

SELECT id, CAST(int_val AS TEXT) AS int_to_text FROM cast_test ORDER BY id;
SELECT id, CAST(text_val AS INT) AS text_to_int FROM cast_test WHERE id = 2;
SELECT id, CAST(float_val AS INT) AS float_to_int FROM cast_test ORDER BY id;

SELECT id, int_val::TEXT AS int_to_text FROM cast_test ORDER BY id;
SELECT id, float_val::INT AS float_to_int FROM cast_test ORDER BY id;
SELECT '123'::INT AS literal_to_int;

SELECT 'hello'::VARCHAR(3) AS truncated;
SELECT 12345::NUMERIC(10,2) AS to_numeric;
SELECT 3.14159::NUMERIC(5,2) AS rounded_numeric;

SELECT CAST('2024-01-15' AS DATE) AS date_cast;
SELECT CAST('2024-01-15 10:30:00' AS TIMESTAMP) AS timestamp_cast;

SELECT 1::BOOLEAN AS one_to_bool;
SELECT 0::BOOLEAN AS zero_to_bool;
SELECT 't'::BOOLEAN AS t_to_bool;
SELECT 'false'::BOOLEAN AS false_to_bool;
SELECT true::INT AS bool_to_int;
SELECT false::INT AS false_to_int;

SELECT '100' + 50 AS implicit_text_to_int;
SELECT 10 + 5.5 AS implicit_int_to_float;

DROP TABLE cast_test;

SELECT 'Type casting tests completed' AS result;

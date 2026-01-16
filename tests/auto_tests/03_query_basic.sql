-- Auto tests: Basic SELECT / WHERE / ORDER / LIMIT
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS query_items;

CREATE TABLE query_items (
    id INT PRIMARY KEY,
    name TEXT,
    price INT
);

INSERT INTO query_items (id, name, price) VALUES
    (1, 'alpha', 10),
    (2, 'beta', 20),
    (3, 'gamma', 30),
    (4, 'delta', 40),
    (5, 'epsilon', 50);

SELECT id, name FROM query_items WHERE price BETWEEN 15 AND 45 ORDER BY id;
SELECT * FROM query_items WHERE name LIKE 'g%';
SELECT * FROM query_items WHERE id IN (1, 3, 5) ORDER BY id DESC;

SELECT * FROM query_items ORDER BY price DESC LIMIT 2;
SELECT * FROM query_items ORDER BY price ASC LIMIT 2 OFFSET 1;
SELECT * FROM query_items ORDER BY id FETCH FIRST 3 ROWS ONLY;

DROP TABLE query_items;

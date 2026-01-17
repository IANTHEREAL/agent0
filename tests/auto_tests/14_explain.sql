-- Auto tests: EXPLAIN
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS explain_items;

CREATE TABLE explain_items (
    id INT PRIMARY KEY,
    val INT
);

INSERT INTO explain_items (id, val) VALUES (1, 10), (2, 20), (3, 30);

EXPLAIN SELECT * FROM explain_items WHERE val > 10;

DROP TABLE explain_items;

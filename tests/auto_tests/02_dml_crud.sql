-- Auto tests: DML / CRUD
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS dml_items;

CREATE TABLE dml_items (
    id INT PRIMARY KEY,
    name TEXT,
    qty INT
);

INSERT INTO dml_items (id, name, qty) VALUES
    (1, 'apple', 10),
    (2, 'banana', 5),
    (3, 'carrot', 7);

UPDATE dml_items SET qty = qty + 1 WHERE id = 2;

INSERT INTO dml_items (id, name, qty)
VALUES (2, 'banana', 99)
ON CONFLICT (id) DO UPDATE SET qty = excluded.qty;

DELETE FROM dml_items WHERE id = 3;

SELECT * FROM dml_items ORDER BY id;

DROP TABLE dml_items;

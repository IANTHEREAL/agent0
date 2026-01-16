-- Auto tests: Transactions and Savepoints
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS txn_items;

CREATE TABLE txn_items (
    id INT PRIMARY KEY,
    val TEXT
);

BEGIN;
INSERT INTO txn_items (id, val) VALUES (1, 'a');
SAVEPOINT sp1;
INSERT INTO txn_items (id, val) VALUES (2, 'b');
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT * FROM txn_items ORDER BY id;

BEGIN;
SELECT * FROM txn_items WHERE id = 1 FOR UPDATE;
COMMIT;

DROP TABLE txn_items;

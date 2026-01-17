-- Auto tests: Constraints and Foreign Keys
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS fk_child;
DROP TABLE IF EXISTS fk_parent;
DROP TABLE IF EXISTS chk_table;

CREATE TABLE fk_parent (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE fk_child (
    id INT PRIMARY KEY,
    parent_id INT,
    value TEXT,
    CONSTRAINT fk_child_parent
        FOREIGN KEY (parent_id) REFERENCES fk_parent(id) ON DELETE CASCADE
);

INSERT INTO fk_parent (id, name) VALUES (1, 'p1');
INSERT INTO fk_child (id, parent_id, value) VALUES (10, 1, 'c1');

DELETE FROM fk_parent WHERE id = 1;
SELECT * FROM fk_child;

CREATE TABLE chk_table (
    id INT PRIMARY KEY,
    amount INT CHECK (amount > 0)
);

INSERT INTO chk_table (id, amount) VALUES (1, 5);
SELECT * FROM chk_table;

DROP TABLE fk_child;
DROP TABLE fk_parent;
DROP TABLE chk_table;

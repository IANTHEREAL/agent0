-- Test: logical DROP COLUMN (PostgreSQL attisdropped semantics)

CREATE TABLE drop_test (id INT PRIMARY KEY, name TEXT, age INT, email TEXT);
INSERT INTO drop_test VALUES (1, 'alice', 30, 'alice@example.com');
INSERT INTO drop_test VALUES (2, 'bob', 25, 'bob@example.com');

-- Drop a middle column
ALTER TABLE drop_test DROP COLUMN age;

-- SELECT * should exclude the dropped column
SELECT * FROM drop_test ORDER BY id;

-- Explicit column references should work for remaining columns
SELECT id, name, email FROM drop_test ORDER BY id;

-- INSERT without column list should skip the dropped column slot
INSERT INTO drop_test VALUES (3, 'charlie', 'charlie@example.com');
SELECT * FROM drop_test ORDER BY id;

-- INSERT with explicit columns should work
INSERT INTO drop_test (id, name, email) VALUES (4, 'dave', 'dave@example.com');
SELECT id, email FROM drop_test WHERE id = 4;

-- Referencing the dropped column should fail
SELECT age FROM drop_test;

-- Qualified reference should also fail
SELECT drop_test.age FROM drop_test;

-- pg_attribute should show attisdropped = true
SELECT attname, attisdropped FROM pg_attribute
WHERE attrelid = 'drop_test'::regclass AND attnum > 0
ORDER BY attnum;

-- DROP COLUMN IF EXISTS on already-dropped column should be a no-op
ALTER TABLE drop_test DROP COLUMN IF EXISTS age;

-- Clean up
DROP TABLE drop_test;

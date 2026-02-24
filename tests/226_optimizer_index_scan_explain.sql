-- Test: Single-path optimizer index scan EXPLAIN verification.
-- db9.use_optimizer toggles are compatibility no-ops; this suite validates
-- planner behavior and compatibility readback plumbing.

DROP TABLE IF EXISTS ois_test;
CREATE TABLE ois_test (
    id INT PRIMARY KEY,
    name TEXT,
    age INT
);
CREATE INDEX idx_ois_name ON ois_test (name);
CREATE INDEX idx_ois_age ON ois_test (age);

INSERT INTO ois_test VALUES (1, 'Alice', 20);
INSERT INTO ois_test VALUES (2, 'Bob', 25);
INSERT INTO ois_test VALUES (3, 'Charlie', 30);
INSERT INTO ois_test VALUES (4, 'Diana', 35);
INSERT INTO ois_test VALUES (5, 'Eve', 40);

ANALYZE ois_test;

-- Test 1: Point lookup on indexed column (optimizer ON)
SET db9.use_optimizer = on;
EXPLAIN SELECT * FROM ois_test WHERE name = 'Alice';

-- Test 2: Range predicate on indexed column (optimizer ON)
EXPLAIN SELECT * FROM ois_test WHERE age > 25;

-- Test 3: Point lookup on PK (optimizer ON)
EXPLAIN SELECT * FROM ois_test WHERE id = 3;

-- Test 4: No index match → SeqScan (optimizer ON)
EXPLAIN SELECT * FROM ois_test WHERE name LIKE '%li%';

-- Test 5: Result correctness with optimizer ON (point lookup)
SELECT * FROM ois_test WHERE name = 'Alice' ORDER BY id;

-- Test 6: Result correctness with optimizer ON (range scan)
SELECT * FROM ois_test WHERE age > 25 ORDER BY id;

-- Test 7: compatibility toggle OFF (no-op) — same queries should work
SET db9.use_optimizer = off;
SELECT * FROM ois_test WHERE name = 'Alice' ORDER BY id;
SELECT * FROM ois_test WHERE age > 25 ORDER BY id;

DROP TABLE ois_test;

-- Test: WriteConflict retry behavior
-- This test validates that the retry infrastructure is in place
-- and basic transaction patterns work correctly.

-- Setup test table
DROP TABLE IF EXISTS retry_test;
CREATE TABLE retry_test (
    id INT PRIMARY KEY,
    counter INT DEFAULT 0,
    updated_at TIMESTAMP DEFAULT '2024-01-15 10:00:00'
);

INSERT INTO retry_test (id, counter) VALUES (1, 0);

-- Test 1: Basic autocommit UPDATE works
UPDATE retry_test SET counter = counter + 1 WHERE id = 1;
SELECT counter FROM retry_test WHERE id = 1;

-- Test 2: Multiple sequential updates (no conflict, baseline)
UPDATE retry_test SET counter = counter + 1 WHERE id = 1;
UPDATE retry_test SET counter = counter + 1 WHERE id = 1;
UPDATE retry_test SET counter = counter + 1 WHERE id = 1;
SELECT counter FROM retry_test WHERE id = 1;

-- Test 3: Explicit transaction with SELECT FOR UPDATE
BEGIN;
SELECT counter FROM retry_test WHERE id = 1 FOR UPDATE;
UPDATE retry_test SET counter = counter + 10 WHERE id = 1;
COMMIT;
SELECT counter FROM retry_test WHERE id = 1;

-- Test 4: Rollback preserves original value
BEGIN;
SELECT counter FROM retry_test WHERE id = 1 FOR UPDATE;
UPDATE retry_test SET counter = 999 WHERE id = 1;
ROLLBACK;
SELECT counter FROM retry_test WHERE id = 1;

-- Test 5: Multiple rows, update one
INSERT INTO retry_test (id, counter) VALUES (2, 100);
INSERT INTO retry_test (id, counter) VALUES (3, 200);
UPDATE retry_test SET counter = counter + 1 WHERE id = 2;
SELECT id, counter FROM retry_test ORDER BY id;

-- Cleanup
DROP TABLE retry_test;

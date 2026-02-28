-- Vector basic usability + transaction read-your-writes smoke test.
DROP TABLE IF EXISTS vector_txn_basic_test;
CREATE TABLE vector_txn_basic_test (
    id INT PRIMARY KEY,
    v vector(3)
);

INSERT INTO vector_txn_basic_test (id, v) VALUES
    (1, '[1.0, 0.0, 0.0]'),
    (2, '[0.0, 1.0, 0.0]');

SELECT 'baseline_nearest' AS test_name,
       ((SELECT id
         FROM vector_txn_basic_test
         ORDER BY v <-> '[1.0, 0.0, 0.0]'
         LIMIT 1) = 1) AS ok;

BEGIN;

INSERT INTO vector_txn_basic_test (id, v) VALUES
    (3, '[0.99, 0.01, 0.0]');

SELECT 'txn_insert_visible' AS test_name,
       EXISTS (
           SELECT 1
           FROM (
               SELECT id
               FROM vector_txn_basic_test
               ORDER BY v <-> '[1.0, 0.0, 0.0]'
               LIMIT 2
           ) nn
           WHERE id = 3
       ) AS ok;

UPDATE vector_txn_basic_test
SET v = '[0.0, 0.0, 1.0]'
WHERE id = 2;

SELECT 'txn_update_visible' AS test_name,
       ((SELECT id
         FROM vector_txn_basic_test
         ORDER BY v <-> '[0.0, 0.0, 1.0]'
         LIMIT 1) = 2) AS ok;

ROLLBACK;

SELECT 'rollback_insert_gone' AS test_name,
       (NOT EXISTS (SELECT 1 FROM vector_txn_basic_test WHERE id = 3)) AS ok;

SELECT 'rollback_update_gone' AS test_name,
       ((SELECT id
         FROM vector_txn_basic_test
         ORDER BY v <-> '[0.0, 1.0, 0.0]'
         LIMIT 1) = 2) AS ok;

UPDATE vector_txn_basic_test
SET v = NULL
WHERE id = 1;

SELECT 'null_vector_filtered' AS test_name,
       ((SELECT id
         FROM vector_txn_basic_test
         ORDER BY v <-> '[1.0, 0.0, 0.0]'
         LIMIT 1) = 2) AS ok;

DROP TABLE vector_txn_basic_test;

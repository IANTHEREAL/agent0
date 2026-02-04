-- ported from pg_tests PR#58: compatible/dangerous_statements.sql
--
-- Validate that UPDATE/DELETE patterns and SELECT ... FOR {UPDATE,SHARE} forms
-- are accepted and return deterministic results.

SET client_min_messages = warning;

-- Cleanup from prior runs.
DROP TABLE IF EXISTS t147_dangerous_statements;

CREATE TABLE t147_dangerous_statements (
  id INT PRIMARY KEY,
  x INT
);

INSERT INTO t147_dangerous_statements (id, x) VALUES
  (1, 1),
  (2, 2);

UPDATE t147_dangerous_statements SET x = 3;
SELECT id, x FROM t147_dangerous_statements ORDER BY id;

UPDATE t147_dangerous_statements SET x = 4 WHERE id = 2;
SELECT id, x FROM t147_dangerous_statements ORDER BY id;

DELETE FROM t147_dangerous_statements WHERE id = 1;
SELECT id, x FROM t147_dangerous_statements ORDER BY id;

-- Locking clauses should not change query results in a single-session test.
SELECT id, x FROM t147_dangerous_statements ORDER BY id FOR UPDATE;

SELECT id, x
FROM t147_dangerous_statements ds
ORDER BY id
FOR SHARE OF ds SKIP LOCKED;

DROP TABLE t147_dangerous_statements;

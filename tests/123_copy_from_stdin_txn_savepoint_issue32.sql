-- Issue #32 regression: COPY FROM STDIN must respect transaction boundaries and savepoints.

DROP TABLE IF EXISTS t_copy_issue32;
CREATE TABLE t_copy_issue32 (id INT PRIMARY KEY);

-- COPY inside explicit transaction should NOT rollback prior statements.
BEGIN;
INSERT INTO t_copy_issue32 VALUES (0);
COPY t_copy_issue32 (id) FROM STDIN;
1
\.
COMMIT;

SELECT id FROM t_copy_issue32 ORDER BY id;

-- SAVEPOINT + COPY + ROLLBACK TO SAVEPOINT should undo COPY effects.
TRUNCATE t_copy_issue32;

BEGIN;
INSERT INTO t_copy_issue32 VALUES (10);
SAVEPOINT sp1;
COPY t_copy_issue32 (id) FROM STDIN;
11
\.
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT id FROM t_copy_issue32 ORDER BY id;

DROP TABLE t_copy_issue32;


-- Issue #32 regression: COPY FROM STDIN must be statement-atomic in autocommit mode.

DROP TABLE IF EXISTS t_copy_autocommit_issue32;
CREATE TABLE t_copy_autocommit_issue32 (id INT PRIMARY KEY);

-- Second row violates PK; autocommit COPY should rollback the whole statement (no partial commit).
COPY t_copy_autocommit_issue32 (id) FROM STDIN;
1
1
\.

SELECT COUNT(*) FROM t_copy_autocommit_issue32;

DROP TABLE t_copy_autocommit_issue32;


DROP TABLE IF EXISTS t_copy_row_mismatch;
CREATE TABLE t_copy_row_mismatch (a INT, b INT);

COPY t_copy_row_mismatch (a, b) FROM STDIN;
1	2
3	4
5
\.

SELECT COUNT(*) FROM t_copy_row_mismatch;

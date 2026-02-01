DROP TABLE IF EXISTS t_copy_blank_lines;
CREATE TABLE t_copy_blank_lines (a INT, b INT);

COPY t_copy_blank_lines (a, b) FROM STDIN;
1	2

3	4
\.

SELECT COUNT(*) FROM t_copy_blank_lines;

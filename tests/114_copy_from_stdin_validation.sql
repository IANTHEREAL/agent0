DROP TABLE IF EXISTS t_copy_validate_type;
CREATE TABLE t_copy_validate_type (a INT NOT NULL);

COPY t_copy_validate_type (a) FROM STDIN;
abc
\.

SELECT COUNT(*) FROM t_copy_validate_type;

DROP TABLE IF EXISTS t_copy_validate_notnull;
CREATE TABLE t_copy_validate_notnull (a INT, b INT NOT NULL);

COPY t_copy_validate_notnull (a) FROM STDIN;
1
\.

SELECT COUNT(*) FROM t_copy_validate_notnull;

DROP TABLE IF EXISTS t_copy_validate_check;
CREATE TABLE t_copy_validate_check (a INT, CONSTRAINT chk_positive CHECK (a > 0));

COPY t_copy_validate_check (a) FROM STDIN;
-1
\.

SELECT COUNT(*) FROM t_copy_validate_check;

DROP TABLE IF EXISTS t_copy_validate_fk_child;
DROP TABLE IF EXISTS t_copy_validate_fk_parent;

CREATE TABLE t_copy_validate_fk_parent (id INT PRIMARY KEY);
CREATE TABLE t_copy_validate_fk_child (
    id INT PRIMARY KEY,
    pid INT,
    CONSTRAINT fk_parent FOREIGN KEY (pid) REFERENCES t_copy_validate_fk_parent(id)
);

COPY t_copy_validate_fk_child (id, pid) FROM STDIN;
1	999
\.

SELECT COUNT(*) FROM t_copy_validate_fk_child;

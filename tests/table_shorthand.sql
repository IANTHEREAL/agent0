-- TABLE <relation> shorthand (PostgreSQL parity)
-- https://github.com/c4pt0r/db9-server/issues/1563

DROP TABLE IF EXISTS tsh_test;
CREATE TABLE tsh_test (id INT, name TEXT);
INSERT INTO tsh_test VALUES (1, 'Alice'), (2, 'Bob');

-- P1: basic TABLE shorthand
TABLE tsh_test ORDER BY id;

-- P2: TABLE ONLY (ONLY is no-op for non-inherited tables)
TABLE ONLY tsh_test ORDER BY id;

-- P3: TABLE ... * (explicit include-children, no-op)
TABLE tsh_test * ORDER BY id;

-- P4: TABLE ONLY ... ORDER BY ... LIMIT (required regression variant)
TABLE ONLY tsh_test ORDER BY id DESC LIMIT 1;

-- P5: TABLE ... * ORDER BY ... LIMIT (required regression variant)
TABLE tsh_test * ORDER BY id DESC LIMIT 1;

-- P6: TABLE ONLY ... * (both modifiers — rejected, PG parity)
TABLE ONLY tsh_test * ORDER BY id;

-- P7: ORDER BY ... OFFSET
TABLE tsh_test ORDER BY id OFFSET 1;

-- P8: case insensitive
table tsh_test ORDER BY id;

-- Cleanup
DROP TABLE tsh_test;

-- Regression: same-transaction scans must reflect UPDATE/UPSERT/DELETE writes
-- exactly once under keyspace mode.

DROP TABLE IF EXISTS t565_user;
DROP TABLE IF EXISTS t565_dept;

CREATE TABLE t565_dept (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE t565_user (
    id INT PRIMARY KEY,
    dept_id INT NOT NULL,
    name TEXT NOT NULL
);

INSERT INTO t565_dept VALUES (1, 'eng'), (2, 'ops');
INSERT INTO t565_user VALUES (1, 1, 'u1'), (2, 2, 'u2');

BEGIN;

UPDATE t565_user SET name = 'u1x' WHERE id = 1;
INSERT INTO t565_user (id, dept_id, name)
VALUES (1, 1, 'u1_upsert')
ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name;
DELETE FROM t565_user WHERE id = 2;

SELECT id, dept_id, name
FROM t565_user
ORDER BY id, name;

SELECT u.id
FROM t565_user u
INNER JOIN t565_dept d ON d.id = u.dept_id
ORDER BY u.id;

ROLLBACK;

DROP TABLE t565_user;
DROP TABLE t565_dept;

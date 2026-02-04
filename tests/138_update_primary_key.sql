-- ported from pg_tests PR#58
--
-- UPDATE primary key columns:
-- - single-column PRIMARY KEY
-- - composite PRIMARY KEY (update one column and both columns)

DROP TABLE IF EXISTS t138_update_pk_single;
DROP TABLE IF EXISTS t138_update_pk_composite;

CREATE TABLE t138_update_pk_single (
    id INT PRIMARY KEY,
    v TEXT
);

INSERT INTO t138_update_pk_single (id, v) VALUES
    (1, 'one'),
    (2, 'two'),
    (3, 'three');

SELECT id, v
FROM t138_update_pk_single
ORDER BY id;

UPDATE t138_update_pk_single SET id = 10 WHERE id = 1;

SELECT id, v
FROM t138_update_pk_single
ORDER BY id;

UPDATE t138_update_pk_single SET id = 12 WHERE id = 2;
UPDATE t138_update_pk_single SET id = 13 WHERE id = 3;

SELECT id, v
FROM t138_update_pk_single
ORDER BY id;

UPDATE t138_update_pk_single SET v = 'one->ten' WHERE id = 10;

SELECT id, v
FROM t138_update_pk_single
ORDER BY id;

CREATE TABLE t138_update_pk_composite (
    a INT,
    b INT,
    v TEXT,
    PRIMARY KEY (a, b)
);

INSERT INTO t138_update_pk_composite (a, b, v) VALUES
    (1, 1, 'r11'),
    (1, 2, 'r12'),
    (2, 1, 'r21');

SELECT a, b, v
FROM t138_update_pk_composite
ORDER BY a, b;

UPDATE t138_update_pk_composite SET b = 99 WHERE a = 1 AND b = 2;

SELECT a, b, v
FROM t138_update_pk_composite
ORDER BY a, b;

UPDATE t138_update_pk_composite SET a = 10, b = 10 WHERE a = 2 AND b = 1;

SELECT a, b, v
FROM t138_update_pk_composite
ORDER BY a, b;

UPDATE t138_update_pk_composite SET v = 'moved' WHERE a = 10 AND b = 10;

SELECT a, b, v
FROM t138_update_pk_composite
ORDER BY a, b;

DROP TABLE t138_update_pk_single;
DROP TABLE t138_update_pk_composite;

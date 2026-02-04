-- ported from pg_tests PR#58
--
-- JOIN ... USING regression coverage:
-- - INNER JOIN USING (single-column)
-- - LEFT JOIN USING (single-column)
-- - (k1, k2) multi-column USING, including LEFT JOIN null-extension

DROP TABLE IF EXISTS t137_join_using_left;
DROP TABLE IF EXISTS t137_join_using_right;
DROP TABLE IF EXISTS t137_join_using_mc_left;
DROP TABLE IF EXISTS t137_join_using_mc_right;

CREATE TABLE t137_join_using_left (
    id INT PRIMARY KEY,
    a TEXT
);

CREATE TABLE t137_join_using_right (
    id INT PRIMARY KEY,
    b TEXT
);

INSERT INTO t137_join_using_left (id, a) VALUES
    (1, 'a1'),
    (2, 'a2'),
    (3, 'a3');

INSERT INTO t137_join_using_right (id, b) VALUES
    (2, 'b2'),
    (3, 'b3'),
    (4, 'b4');

SELECT id, l.id AS l_id, r.id AS r_id, a, b
FROM t137_join_using_left AS l
JOIN t137_join_using_right AS r USING (id)
ORDER BY id;

SELECT id, l.id AS l_id, r.id AS r_id, a, b
FROM t137_join_using_left AS l
LEFT JOIN t137_join_using_right AS r USING (id)
ORDER BY id;

CREATE TABLE t137_join_using_mc_left (
    k1 INT,
    k2 INT,
    c TEXT,
    PRIMARY KEY (k1, k2)
);

CREATE TABLE t137_join_using_mc_right (
    k1 INT,
    k2 INT,
    d TEXT,
    PRIMARY KEY (k1, k2)
);

INSERT INTO t137_join_using_mc_left (k1, k2, c) VALUES
    (1, 1, 'c11'),
    (1, 2, 'c12'),
    (2, 1, 'c21');

INSERT INTO t137_join_using_mc_right (k1, k2, d) VALUES
    (1, 1, 'd11'),
    (2, 1, 'd21'),
    (2, 2, 'd22');

SELECT k1, k2,
       l.k1 AS l_k1, l.k2 AS l_k2,
       r.k1 AS r_k1, r.k2 AS r_k2,
       c, d
FROM t137_join_using_mc_left AS l
JOIN t137_join_using_mc_right AS r USING (k1, k2)
ORDER BY k1, k2;

SELECT k1, k2,
       l.k1 AS l_k1, l.k2 AS l_k2,
       r.k1 AS r_k1, r.k2 AS r_k2,
       c, d
FROM t137_join_using_mc_left AS l
LEFT JOIN t137_join_using_mc_right AS r USING (k1, k2)
ORDER BY k1, k2;

DROP TABLE t137_join_using_left;
DROP TABLE t137_join_using_right;
DROP TABLE t137_join_using_mc_left;
DROP TABLE t137_join_using_mc_right;

-- Regression test for issue #220: UPDATE ... FROM should not panic when FROM-side is empty.

DROP TABLE IF EXISTS t_update_from_target;
DROP TABLE IF EXISTS t_update_from_src;

CREATE TABLE t_update_from_target (
    id INT PRIMARY KEY,
    v INT
);

CREATE TABLE t_update_from_src (
    id INT PRIMARY KEY
);

INSERT INTO t_update_from_target VALUES (1, 10);

-- FROM is empty: should affect 0 rows and RETURNING should be empty.
UPDATE t_update_from_target AS t
SET v = 11
FROM t_update_from_src AS s
RETURNING t.id, t.v;

-- Verify target table unchanged.
SELECT * FROM t_update_from_target ORDER BY id;

DROP TABLE t_update_from_target;
DROP TABLE t_update_from_src;

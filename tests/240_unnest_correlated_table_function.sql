-- Correlated table-function arguments require LATERAL execution.

DROP TABLE IF EXISTS unnest_arr_src;

CREATE TABLE unnest_arr_src(id INT PRIMARY KEY, arr INT[]);
INSERT INTO unnest_arr_src VALUES (1, ARRAY[10,20]), (2, ARRAY[30]);

SELECT s.id, u.val
FROM unnest_arr_src s
JOIN UNNEST(s.arr) AS u(val) ON true;

DROP TABLE unnest_arr_src;

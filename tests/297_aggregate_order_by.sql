DROP TABLE IF EXISTS t297_aggregate_order_by;
CREATE TABLE t297_aggregate_order_by (grp INT, v TEXT, ord INT);

INSERT INTO t297_aggregate_order_by(grp, v, ord) VALUES
  (1, 'c', 3),
  (1, 'a', 1),
  (1, 'b', 2),
  (1, NULL, 0),
  (1, 'aa', 1),
  (1, 'n', NULL),
  (2, 'x', 2),
  (2, 'z', 3),
  (2, 'y', 1),
  (2, NULL, 4);

SELECT 'string_asc=' || string_agg(v, ',' ORDER BY ord, v)
FROM t297_aggregate_order_by
WHERE grp = 1;

SELECT 'string_desc=' || string_agg(v, ',' ORDER BY ord DESC NULLS LAST, v DESC)
FROM t297_aggregate_order_by
WHERE grp = 1;

SELECT 'string_nulls_first=' || string_agg(v, ',' ORDER BY ord NULLS FIRST, v)
FROM t297_aggregate_order_by
WHERE grp = 1;

SELECT grp, string_agg(v, ',' ORDER BY ord, v) AS grouped_order
FROM t297_aggregate_order_by
GROUP BY grp
ORDER BY grp;

SELECT 'array_asc=' || array_to_string(array_agg(v ORDER BY ord, v), ',', '<null>')
FROM t297_aggregate_order_by
WHERE grp = 1;

-- Error path: ORDER BY key inside aggregate must resolve in current scope.
SELECT string_agg(v, ',' ORDER BY missing_col)
FROM t297_aggregate_order_by;

DROP TABLE t297_aggregate_order_by;

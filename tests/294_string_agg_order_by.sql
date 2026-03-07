DROP TABLE IF EXISTS t294_string_agg;
CREATE TABLE t294_string_agg (grp int, v text, ord int);

INSERT INTO t294_string_agg(grp, v, ord) VALUES
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

SELECT string_agg(v, ',' ORDER BY ord, v) AS asc_order
FROM t294_string_agg
WHERE grp = 1;

SELECT string_agg(v, ',' ORDER BY ord DESC NULLS LAST, v DESC) AS desc_order
FROM t294_string_agg
WHERE grp = 1;

SELECT string_agg(v, ',' ORDER BY ord NULLS FIRST, v) AS nulls_first_order
FROM t294_string_agg
WHERE grp = 1;

SELECT grp, string_agg(v, ',' ORDER BY ord, v) AS grouped_order
FROM t294_string_agg
GROUP BY grp
ORDER BY grp;

-- Analyzer/executor error path: ORDER BY references an unknown column.
SELECT string_agg(v, ',' ORDER BY missing_col)
FROM t294_string_agg;

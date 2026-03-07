DROP TABLE IF EXISTS t1573_string_agg_null_delimiter;
CREATE TABLE t1573_string_agg_null_delimiter (grp INT, v TEXT, ord INT);

INSERT INTO t1573_string_agg_null_delimiter(grp, v, ord) VALUES
  (1, 'b', 2),
  (1, 'a', 1),
  (1, 'c', 3),
  (1, NULL, 0),
  (1, 'a', 4),
  (2, 'x', 2),
  (2, 'z', 3),
  (2, 'y', 1),
  (2, NULL, 4),
  (2, 'x', 5);

SELECT 'null_basic=' || string_agg(v, NULL)
FROM (VALUES ('a'), ('b'), (NULL), ('c')) AS t(v);

SELECT 'null_cast_short=' || string_agg(v, NULL::text ORDER BY v)
FROM (VALUES ('a'), ('b'), (NULL), ('c')) AS t(v);

SELECT 'null_cast_explicit=' || string_agg(v, CAST(NULL AS text) ORDER BY v)
FROM (VALUES ('a'), ('b'), (NULL), ('c')) AS t(v);

SELECT 'null_order_asc=' || string_agg(v, NULL ORDER BY ord, v)
FROM t1573_string_agg_null_delimiter
WHERE grp = 1;

SELECT 'null_order_desc=' || string_agg(v, NULL ORDER BY ord DESC NULLS LAST, v DESC)
FROM t1573_string_agg_null_delimiter
WHERE grp = 1;

SELECT grp, string_agg(v, NULL ORDER BY ord, v) AS grouped_null
FROM t1573_string_agg_null_delimiter
GROUP BY grp
ORDER BY grp;

SELECT 'null_distinct=' || string_agg(DISTINCT v, NULL ORDER BY v)
FROM t1573_string_agg_null_delimiter
WHERE grp = 1;

SELECT 'mixed=' ||
       string_agg(v, NULL ORDER BY ord, v) ||
       ';' ||
       string_agg(v, ',' ORDER BY ord, v)
FROM t1573_string_agg_null_delimiter
WHERE grp = 2;

DROP TABLE t1573_string_agg_null_delimiter;

DROP TABLE IF EXISTS t294;
CREATE TABLE t294 (grp INT, v TEXT, ord INT);
INSERT INTO t294(grp, v, ord) VALUES
  (1, 'c', 3), (1, 'a', 1), (1, 'b', 2),
  (2, 'x', 2), (2, 'z', 3), (2, 'y', 1);

-- Positive: DISTINCT + ORDER BY on argument column (should succeed)
SELECT string_agg(DISTINCT v, ',' ORDER BY v) FROM t294 WHERE grp = 1;
-- Positive: DISTINCT arg is no-op cast; ORDER BY uses base expression
SELECT string_agg(DISTINCT v::text, ',' ORDER BY v) FROM t294 WHERE grp = 1;
-- Positive: DISTINCT arg is base expression; ORDER BY uses no-op cast
SELECT string_agg(DISTINCT v, ',' ORDER BY v::text) FROM t294 WHERE grp = 1;
-- Positive: DESC
SELECT string_agg(DISTINCT v, ',' ORDER BY v DESC) FROM t294 WHERE grp = 1;
-- Negative: ORDER BY delimiter constant (must not match delimiter arg slot)
SELECT string_agg(DISTINCT v, ',' ORDER BY ',') FROM t294 WHERE grp = 1;
-- Negative: ORDER BY non-argument column (must error)
SELECT string_agg(DISTINCT v, ',' ORDER BY ord) FROM t294;
-- Negative: array_agg DISTINCT + ORDER BY non-argument
SELECT array_agg(DISTINCT v ORDER BY ord) FROM t294;
-- Positive: non-DISTINCT ORDER BY on any column (no restriction)
SELECT string_agg(v, ',' ORDER BY ord) FROM t294 WHERE grp = 1;

DROP TABLE t294;

-- ported from pg_tests PR#58: compatible/cross_join.sql
--
-- Correlated subquery with an extra FROM item.

SET client_min_messages = warning;

DROP TABLE IF EXISTS t139_cross_join;

CREATE TABLE t139_cross_join (c0 INT);
INSERT INTO t139_cross_join (c0) VALUES (1);

SELECT (
  SELECT count(t2.c0) FROM t139_cross_join t2
  WHERE ((t1.c0) IN (SELECT max(t3.c0) FROM t139_cross_join t3))
) AS result
FROM t139_cross_join t1;

DROP TABLE t139_cross_join;

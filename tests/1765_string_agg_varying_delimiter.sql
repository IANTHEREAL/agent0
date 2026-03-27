-- Regression test for #1765: string_agg must evaluate delimiter per-row.
-- PostgreSQL 17 returns per-row delimiters; db9 previously used only the first.

DROP TABLE IF EXISTS t1765;
CREATE TABLE t1765 (v TEXT, d TEXT, ord INT);
INSERT INTO t1765 VALUES ('a', ',', 1), ('b', ';', 2), ('c', '|', 3);

-- Core repro: column-reference delimiter with ORDER BY
SELECT string_agg(v, d ORDER BY ord) FROM t1765;

-- Without ORDER BY (insertion order may vary, but delimiter must still be per-row)
-- Use a subquery with explicit ordering to get deterministic input.
SELECT string_agg(v, d) FROM (SELECT * FROM t1765 ORDER BY ord) sub;

-- NULL delimiter on some rows — PG treats NULL as empty string (no separator)
INSERT INTO t1765 VALUES ('d', NULL, 4);
SELECT string_agg(v, d ORDER BY ord) FROM t1765;

-- All delimiters NULL
SELECT string_agg(v, NULL ORDER BY ord) FROM t1765;

-- FILTER with varying delimiter
SELECT string_agg(v, d ORDER BY ord) FILTER (WHERE ord > 1) FROM t1765;

-- Grouped with varying delimiter
INSERT INTO t1765 VALUES ('x', '-', 1), ('y', '~', 2);
SELECT ord % 2 AS grp, string_agg(v, d ORDER BY ord) FROM t1765 GROUP BY ord % 2 ORDER BY grp;

DROP TABLE t1765;

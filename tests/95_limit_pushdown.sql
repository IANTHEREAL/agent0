-- Repro for #53: index scan + LIMIT should not scan/fetch all rows.
-- Table/data are prepared by 95_limit_pushdown_setup.sql + 95_limit_pushdown_load.py.

EXPLAIN (ANALYZE) SELECT * FROM limit_pushdown_t WHERE a = 1 LIMIT 5;

DROP TABLE limit_pushdown_t;


-- Nullable aggregation: SUM/AVG/MIN/MAX must return NULL for all-null groups.
-- PostgreSQL null-handling semantics for aggregate functions.

DROP TABLE IF EXISTS t_null_agg CASCADE;

CREATE TABLE t_null_agg (
    id SERIAL PRIMARY KEY,
    grp TEXT NOT NULL,
    val INT
);

INSERT INTO t_null_agg (grp, val) VALUES
    ('a', 10),
    ('a', NULL),
    ('a', 30),
    ('b', NULL),
    ('b', NULL);

-- Mixed null group: non-null aggregate results.
SELECT grp, SUM(val) AS s, AVG(val) AS a, MIN(val) AS mn, MAX(val) AS mx, COUNT(val) AS c
FROM t_null_agg WHERE grp = 'a' GROUP BY grp;

-- All-null group: SUM/AVG/MIN/MAX return NULL, COUNT returns 0.
SELECT grp, SUM(val) AS s, AVG(val) AS a, MIN(val) AS mn, MAX(val) AS mx, COUNT(val) AS c
FROM t_null_agg WHERE grp = 'b' GROUP BY grp;

-- Global aggregate over all-null: same semantics.
SELECT SUM(val) AS s, AVG(val) AS a, MIN(val) AS mn, MAX(val) AS mx, COUNT(val) AS c
FROM t_null_agg WHERE grp = 'b';

DROP TABLE t_null_agg CASCADE;

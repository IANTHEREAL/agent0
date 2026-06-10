-- Regression coverage for #2555: HashAggregate must consume input rows
-- incrementally while preserving aggregate semantics.

DROP TABLE IF EXISTS t2555_agg;
CREATE TABLE t2555_agg (
    id INT PRIMARY KEY,
    grp TEXT,
    v INT,
    label TEXT,
    delim TEXT,
    ord INT,
    keep BOOLEAN,
    payload TEXT
);

INSERT INTO t2555_agg VALUES
    (1, 'A', 10, 'alpha', ',', 1, true,  'small'),
    (2, 'A', 20, 'beta',  ';', 2, false, 'small'),
    (3, 'A', NULL, 'alpha', '|', 3, true,  'small'),
    (4, 'B', 5,  'gamma', '-', 1, true,  'small'),
    (5, 'B', 15, 'delta', '~', 2, true,  'small'),
    (6, 'B', 25, 'gamma', NULL, 3, false, 'small'),
    (7, NULL, 99, 'zeta', ':', 1, true,  'small');

-- Basic grouped aggregate semantics, including NULL group keys and NULL inputs.
SELECT
    COALESCE(grp, '<null>') AS grp_key,
    COUNT(*) AS cnt_all,
    COUNT(v) AS cnt_v,
    SUM(v) AS sum_v,
    MIN(v) AS min_v,
    MAX(v) AS max_v
FROM t2555_agg
GROUP BY grp
ORDER BY grp_key;

-- Empty input without GROUP BY still returns exactly one aggregate row.
SELECT
    'empty_global=' ||
    COUNT(*)::TEXT || ',' ||
    COUNT(v)::TEXT || ',' ||
    COALESCE(SUM(v)::TEXT, 'NULL') AS empty_global
FROM t2555_agg
WHERE false;

-- Empty input with GROUP BY returns no aggregate groups.
SELECT 'empty_group_rows=' || COUNT(*)::TEXT AS empty_group_rows
FROM (
    SELECT grp, COUNT(*) FROM t2555_agg WHERE false GROUP BY grp
) s;

-- FILTER and DISTINCT state must remain per aggregate/per group.
SELECT
    COALESCE(grp, '<null>') AS grp_key,
    COUNT(*) FILTER (WHERE keep) AS kept_rows,
    SUM(v) FILTER (WHERE keep) AS kept_sum,
    COUNT(DISTINCT label) AS distinct_labels
FROM t2555_agg
GROUP BY grp
ORDER BY grp_key;

-- Ordered string_agg must still buffer per group, sort by ORDER BY, and use
-- each row's delimiter after sorting.
SELECT
    COALESCE(grp, '<null>') AS grp_key,
    string_agg(label, delim ORDER BY ord DESC NULLS LAST, label) AS labels_desc
FROM t2555_agg
GROUP BY grp
ORDER BY grp_key;

-- Wide retained aggregate state should remain semantically correct. This covers
-- the HashAggregate state-retention shape; table scans may still buffer rows
-- independently and require separate streaming-scan work.
DROP TABLE IF EXISTS t2555_wide;
CREATE TABLE t2555_wide (id INT PRIMARY KEY, grp INT, payload TEXT);
INSERT INTO t2555_wide
SELECT g, 1, repeat('x', 4096)
FROM generate_series(1, 64) AS g;

SELECT
    'wide_count=' || COUNT(*)::TEXT ||
    ',max_payload=' || MAX(length(payload))::TEXT ||
    ',string_agg_len=' || length(string_agg(payload, ''))::TEXT AS wide_probe
FROM t2555_wide
GROUP BY grp;

DROP TABLE t2555_wide;
DROP TABLE t2555_agg;

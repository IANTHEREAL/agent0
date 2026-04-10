-- DB9 cop pushdown: secondary-index row fetch keeps PG-shaped EXPLAIN and adds DB9 Cop annotations.

DROP TABLE IF EXISTS ab_pushdown_demo;
DROP TABLE IF EXISTS ab_pushdown_demo_on;
DROP TABLE IF EXISTS ab_pushdown_demo_off;

CREATE TABLE ab_pushdown_demo(
    id INT PRIMARY KEY,
    a INT NOT NULL,
    b INT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX ab_pushdown_demo_ab_idx ON ab_pushdown_demo(a, b);

INSERT INTO ab_pushdown_demo
SELECT i, i % 5000, i % 7, 'payload-' || i::text
FROM generate_series(1, 5000) AS gs(i);
INSERT INTO ab_pushdown_demo VALUES (6001, 1234, 1, 'target-hit');
ANALYZE ab_pushdown_demo;

SET db9.enable_cop_pushdown = on;
SELECT 'pushdown_on' AS phase;
EXPLAIN
SELECT id, payload
FROM ab_pushdown_demo
WHERE a = 1234 AND b = abs(-1);
SELECT id, payload
FROM ab_pushdown_demo
WHERE a = 1234 AND b = abs(-1);
CREATE TEMP TABLE ab_pushdown_demo_on AS
SELECT id, payload
FROM ab_pushdown_demo
WHERE a = 1234 AND b = abs(-1);

SET db9.enable_cop_pushdown = off;
SELECT 'pushdown_off' AS phase;
EXPLAIN
SELECT id, payload
FROM ab_pushdown_demo
WHERE a = 1234 AND b = abs(-1);
CREATE TEMP TABLE ab_pushdown_demo_off AS
SELECT id, payload
FROM ab_pushdown_demo
WHERE a = 1234 AND b = abs(-1);

SELECT 'secondary_index_row_fetch_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_demo_on
            EXCEPT ALL
            SELECT * FROM ab_pushdown_demo_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_demo_off
            EXCEPT ALL
            SELECT * FROM ab_pushdown_demo_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE ab_pushdown_demo;
DROP TABLE ab_pushdown_demo_on;
DROP TABLE ab_pushdown_demo_off;

-- COPY-written rows must stay visible through secondary-index scans on both
-- pushdown-on and pushdown-off paths.

DROP TABLE IF EXISTS ab_pushdown_copy_secondary_idx;
DROP TABLE IF EXISTS ab_pushdown_copy_secondary_idx_on_eq;
DROP TABLE IF EXISTS ab_pushdown_copy_secondary_idx_on_in;
DROP TABLE IF EXISTS ab_pushdown_copy_secondary_idx_off_eq;
DROP TABLE IF EXISTS ab_pushdown_copy_secondary_idx_off_in;

CREATE TABLE ab_pushdown_copy_secondary_idx(
    id INT PRIMARY KEY,
    k INT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX ab_pushdown_copy_secondary_idx_k_idx ON ab_pushdown_copy_secondary_idx(k);

INSERT INTO ab_pushdown_copy_secondary_idx
SELECT i, i, 'base-' || i::text
FROM generate_series(1, 5000) AS gs(i);

COPY ab_pushdown_copy_secondary_idx (id, k, payload) FROM STDIN;
6001	777777	copied-target
\.

ANALYZE ab_pushdown_copy_secondary_idx;

SET db9.enable_cop_pushdown = on;
SELECT 'pushdown_on_eq' AS phase;
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k = 777777
ORDER BY id;
CREATE TEMP TABLE ab_pushdown_copy_secondary_idx_on_eq AS
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k = 777777
ORDER BY id;

SELECT 'pushdown_on_in' AS phase;
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k IN (42, 777777)
ORDER BY id;
CREATE TEMP TABLE ab_pushdown_copy_secondary_idx_on_in AS
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k IN (42, 777777)
ORDER BY id;

SET db9.enable_cop_pushdown = off;
SELECT 'pushdown_off_eq' AS phase;
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k = 777777
ORDER BY id;
CREATE TEMP TABLE ab_pushdown_copy_secondary_idx_off_eq AS
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k = 777777
ORDER BY id;

SELECT 'pushdown_off_in' AS phase;
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k IN (42, 777777)
ORDER BY id;
CREATE TEMP TABLE ab_pushdown_copy_secondary_idx_off_in AS
SELECT id, payload
FROM ab_pushdown_copy_secondary_idx
WHERE k IN (42, 777777)
ORDER BY id;

SELECT 'copy_secondary_eq_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_copy_secondary_idx_on_eq
            EXCEPT ALL
            SELECT * FROM ab_pushdown_copy_secondary_idx_off_eq
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_copy_secondary_idx_off_eq
            EXCEPT ALL
            SELECT * FROM ab_pushdown_copy_secondary_idx_on_eq
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'copy_secondary_in_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_copy_secondary_idx_on_in
            EXCEPT ALL
            SELECT * FROM ab_pushdown_copy_secondary_idx_off_in
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM ab_pushdown_copy_secondary_idx_off_in
            EXCEPT ALL
            SELECT * FROM ab_pushdown_copy_secondary_idx_on_in
        ) AS right_only
    ) AS mismatch_count
) AS parity;

DROP TABLE ab_pushdown_copy_secondary_idx;
DROP TABLE ab_pushdown_copy_secondary_idx_on_eq;
DROP TABLE ab_pushdown_copy_secondary_idx_on_in;
DROP TABLE ab_pushdown_copy_secondary_idx_off_eq;
DROP TABLE ab_pushdown_copy_secondary_idx_off_in;

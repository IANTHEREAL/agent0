-- Auto tests: Edge cases (NULLs, DISTINCT ON, CASE)
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS edge_items;

CREATE TABLE edge_items (
    id INT,
    grp TEXT,
    val INT
);

INSERT INTO edge_items (id, grp, val) VALUES
    (1, 'a', 10),
    (2, 'a', NULL),
    (3, 'b', 5),
    (4, 'b', 5),
    (5, 'c', NULL);

SELECT DISTINCT ON (grp) grp, id, val
FROM edge_items
ORDER BY grp, id;

SELECT id, COALESCE(val, 0) AS val_coalesced FROM edge_items ORDER BY id;
SELECT id, NULLIF(val, 5) AS val_nullif FROM edge_items ORDER BY id;

SELECT id,
       CASE WHEN val IS NULL THEN 'missing' ELSE 'present' END AS val_state
FROM edge_items
ORDER BY id;

DROP TABLE edge_items;

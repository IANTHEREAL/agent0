-- Regression: correlated LATERAL subquery without FROM clause must return
-- one row per outer row (previously only returned the first row).

DROP TABLE IF EXISTS lat_nf;
CREATE TABLE lat_nf(id INT PRIMARY KEY, name TEXT);
INSERT INTO lat_nf VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');

-- Correlated LATERAL with LENGTH (no FROM in subquery)
SELECT t.name, q.len
FROM lat_nf t,
     LATERAL (SELECT LENGTH(t.name) AS len) q
ORDER BY t.id;

-- Correlated LATERAL with string concat
SELECT t.name, q.label
FROM lat_nf t,
     LATERAL (SELECT t.name || '-' || t.id::text AS label) q
ORDER BY t.id;

-- Uncorrelated LATERAL (regression: must still work)
SELECT t.name, q.val
FROM lat_nf t,
     LATERAL (SELECT 42 AS val) q
ORDER BY t.id;

DROP TABLE lat_nf;

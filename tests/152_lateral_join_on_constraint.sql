-- Regression: LATERAL-derived JOIN must respect JOIN constraints (ON).

DROP TABLE IF EXISTS lat_on_t;

CREATE TABLE lat_on_t(id INT PRIMARY KEY);
INSERT INTO lat_on_t VALUES (1);

-- LEFT JOIN + ON false => NULL-extend right side (do not return lateral rows).
SELECT t.id, l.x
FROM lat_on_t t
LEFT JOIN LATERAL (SELECT 1 AS x) l ON false
ORDER BY t.id;

-- INNER JOIN + ON false => 0 rows.
SELECT t.id, l.x
FROM lat_on_t t
JOIN LATERAL (SELECT 1 AS x) l ON false
ORDER BY t.id;

DROP TABLE lat_on_t;


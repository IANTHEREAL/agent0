-- Regression: LATERAL join executor must not swallow rewrite/eval errors.

DROP TABLE IF EXISTS lat_err_outer;
DROP TABLE IF EXISTS lat_err_right;

CREATE TABLE lat_err_outer(id INT);
CREATE TABLE lat_err_right(id INT);

INSERT INTO lat_err_outer VALUES (1);
INSERT INTO lat_err_right VALUES (1);

-- Ambiguous unqualified identifier in a JOIN condition after a LATERAL join must error.
SELECT 1 FROM lat_err_outer o JOIN LATERAL (SELECT o.id) l ON true JOIN lat_err_right r ON id = r.id;

-- WHERE must enforce boolean context (WHERE 1 should error, not silently filter to empty).
SELECT o.id FROM lat_err_outer o JOIN LATERAL (SELECT 1) l ON true WHERE 1;

-- ORDER BY missing column must error (not treat as NULL and succeed).
SELECT o.id FROM lat_err_outer o JOIN LATERAL (SELECT 1) l ON true ORDER BY missing_col;

DROP TABLE lat_err_outer;
DROP TABLE lat_err_right;


-- PRD-D03: CURRENT_TIMESTAMP(p) / NOW(p) precision and output.

-- CURRENT_TIMESTAMP(p) must respect precision and be stable within a statement.
SELECT (CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP)) AS ok;

-- NOW(p) is accepted for compatibility (same as CURRENT_TIMESTAMP(p)).
SELECT (NOW(0) = CURRENT_TIMESTAMP(0)) AS ok;

-- DEFAULT CURRENT_TIMESTAMP(0) should truncate to whole seconds.
CREATE TABLE ts_precision (
    id INT PRIMARY KEY,
    ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP(0)
);

INSERT INTO ts_precision(id) VALUES (1);

-- Casting to text must produce a formatted timestamp without fractional seconds.
SELECT (ts::text LIKE '____-__-__ __:__:__' AND ts::text NOT LIKE '%.%') AS ok
FROM ts_precision
ORDER BY id;

DROP TABLE ts_precision;

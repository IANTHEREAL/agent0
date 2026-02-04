-- ported from pg_tests PR#58
--
-- AND/OR boolean expression precedence with NULLIF() division to avoid divide-by-zero.

DROP TABLE IF EXISTS t136_and_or_nullif;

CREATE TABLE t136_and_or_nullif (
    id INT PRIMARY KEY,
    a INT,
    num INT NOT NULL,
    denom INT
);

INSERT INTO t136_and_or_nullif (id, a, num, denom) VALUES
    (1, 1, 10, 2),     -- safe_div=5, matches=true
    (2, 1, 10, 0),     -- safe_div=NULL
    (3, 2, -4, 2),     -- safe_div=-2, matches=true
    (4, 2, 1, 0),      -- safe_div=NULL
    (5, 3, 3, 1),      -- safe_div=3, matches=false
    (6, NULL, 10, 2);  -- a=NULL

SELECT
    id,
    a,
    num,
    denom,
    num / NULLIF(denom, 0) AS safe_div,
    (a = 1 AND num / NULLIF(denom, 0) > 3 OR a = 2 AND num / NULLIF(denom, 0) < 0) AS matches
FROM t136_and_or_nullif
ORDER BY id;

SELECT id
FROM t136_and_or_nullif
WHERE a = 1 AND num / NULLIF(denom, 0) > 3 OR a = 2 AND num / NULLIF(denom, 0) < 0
ORDER BY id;

DROP TABLE t136_and_or_nullif;

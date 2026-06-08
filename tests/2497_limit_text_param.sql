-- #2497: parameterised / text LIMIT and OFFSET must coerce to bigint like PG.
-- PostgreSQL accepts a text-valued LIMIT/OFFSET (e.g. the simple-query
-- `LIMIT '1'` form, or a parameter bound as text) and coerces it to bigint.
-- These cases previously failed on db9 with
-- "LIMIT must evaluate to a non-negative integer, got: Text(...)".

DROP TABLE IF EXISTS limit_param;
CREATE TABLE limit_param (n INT);
INSERT INTO limit_param (n) VALUES (1), (2), (3), (4), (5);

-- Accepted: text integer coerces to bigint.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '2';

-- Accepted: text OFFSET.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n OFFSET '3';

-- Accepted: surrounding whitespace is trimmed for bigint input.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '  2  ';

-- Accepted: LIMIT NULL ≡ LIMIT ALL (no bound).
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT NULL;

-- Accepted: text LIMIT + text OFFSET together.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '2' OFFSET '1';

-- Error: non-integer text is invalid bigint input syntax.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '1.5';

-- Error: non-numeric text is invalid bigint input syntax.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT 'abc';

-- Error: out-of-range bigint (i64::MAX + 1) is distinct from invalid syntax.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '9223372036854775808';

-- Error: negative LIMIT is rejected with a clause-specific message.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n LIMIT '-1';

-- Error: negative OFFSET is rejected with a clause-specific message.
SELECT n, n * 10 AS n10 FROM limit_param ORDER BY n OFFSET '-1';

DROP TABLE limit_param;

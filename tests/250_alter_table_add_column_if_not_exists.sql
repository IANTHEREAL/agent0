-- ALTER TABLE ADD COLUMN IF NOT EXISTS: silent no-op when column exists.
-- Regression test for issue #1061.

DROP TABLE IF EXISTS t250_ine CASCADE;

CREATE TABLE t250_ine (id INT PRIMARY KEY, name TEXT);
INSERT INTO t250_ine VALUES (1, 'a'), (2, 'b');

-- 1) ADD COLUMN IF NOT EXISTS on a new column — succeeds normally
ALTER TABLE t250_ine ADD COLUMN IF NOT EXISTS extra INT;

SELECT id, name, extra FROM t250_ine ORDER BY id;

-- 2) ADD COLUMN IF NOT EXISTS on an existing column — silent no-op
ALTER TABLE t250_ine ADD COLUMN IF NOT EXISTS extra INT;

SELECT id, name, extra FROM t250_ine ORDER BY id;

-- 3) ADD COLUMN without IF NOT EXISTS on existing column — error (separate .errors test)

DROP TABLE t250_ine;

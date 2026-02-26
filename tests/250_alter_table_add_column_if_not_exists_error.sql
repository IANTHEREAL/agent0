-- ALTER TABLE ADD COLUMN without IF NOT EXISTS errors on duplicate column.
-- Companion error-path test for issue #1061.

DROP TABLE IF EXISTS t250_ine_err CASCADE;

CREATE TABLE t250_ine_err (id INT PRIMARY KEY, name TEXT);

-- Should error: column "name" already exists
ALTER TABLE t250_ine_err ADD COLUMN name TEXT;

DROP TABLE IF EXISTS t250_ine_err;

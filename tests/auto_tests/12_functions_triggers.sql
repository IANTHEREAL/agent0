-- Auto tests: Functions and Triggers (DDL only)
-- Note: CREATE PROCEDURE syntax differs between PostgreSQL and TiPG, omitted here for cross-compat.
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TRIGGER IF EXISTS trg_before_insert ON trg_table;
DROP FUNCTION IF EXISTS trg_fn();
DROP FUNCTION IF EXISTS add_one(INT);
DROP TABLE IF EXISTS trg_table;

CREATE TABLE trg_table (
    id INT PRIMARY KEY,
    val INT
);

CREATE FUNCTION add_one(i INT) RETURNS INT
LANGUAGE SQL
AS 'SELECT i + 1';

CREATE OR REPLACE FUNCTION trg_fn() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_before_insert
BEFORE INSERT ON trg_table
FOR EACH ROW
EXECUTE FUNCTION trg_fn();

DROP TRIGGER trg_before_insert ON trg_table;
DROP FUNCTION trg_fn();
DROP FUNCTION add_one(INT);
DROP TABLE trg_table;

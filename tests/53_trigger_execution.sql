DROP TRIGGER IF EXISTS set_updated_at ON trigger_test;
DROP TABLE IF EXISTS trigger_test;
DROP FUNCTION IF EXISTS set_updated_at();

CREATE TABLE trigger_test (
    id SERIAL PRIMARY KEY,
    name TEXT,
    updated_at TIMESTAMP
);

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at := NOW();
  RETURN NEW;
END;
$$;

CREATE TRIGGER set_updated_at BEFORE INSERT OR UPDATE ON trigger_test
FOR EACH ROW EXECUTE PROCEDURE set_updated_at();

INSERT INTO trigger_test (name) VALUES ('alice');

SELECT 'INSERT_HAS_TIMESTAMP=' || CASE WHEN updated_at IS NOT NULL THEN 'true' ELSE 'false' END
FROM trigger_test WHERE name = 'alice';

UPDATE trigger_test SET name = 'alice_updated' WHERE name = 'alice';

SELECT 'UPDATE_HAS_TIMESTAMP=' || CASE WHEN updated_at IS NOT NULL THEN 'true' ELSE 'false' END
FROM trigger_test WHERE name = 'alice_updated';

DROP TRIGGER set_updated_at ON trigger_test;
DROP TABLE trigger_test;
DROP FUNCTION set_updated_at();

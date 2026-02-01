-- Stage 1: CREATE FUNCTION/TRIGGER should be accepted and stored (no execution semantics).

DROP TABLE IF EXISTS ft_t;
DROP FUNCTION IF EXISTS set_updated_at();

CREATE TABLE ft_t (id INT PRIMARY KEY, updated_at TIMESTAMP);

-- CREATE TRIGGER should fail fast if the referenced function is missing.
CREATE TRIGGER set_updated_at BEFORE UPDATE ON ft_t
FOR EACH ROW EXECUTE PROCEDURE set_updated_at();

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at = NOW();
  RETURN NEW;
END;
$$;

SELECT 'FUNC_FOUND=' || count(*) FROM pg_catalog.pg_proc WHERE proname = 'set_updated_at';

CREATE TRIGGER set_updated_at BEFORE UPDATE ON ft_t
FOR EACH ROW EXECUTE PROCEDURE set_updated_at();

SELECT 'TRIGGER_FOUND=' || count(*) FROM pg_catalog.pg_trigger WHERE tgname = 'set_updated_at';

DROP TRIGGER set_updated_at ON ft_t;
SELECT 'TRIGGER_LEFT=' || count(*) FROM pg_catalog.pg_trigger WHERE tgname = 'set_updated_at';

DROP FUNCTION set_updated_at();
SELECT 'FUNC_LEFT=' || count(*) FROM pg_catalog.pg_proc WHERE proname = 'set_updated_at';

DROP TABLE ft_t;

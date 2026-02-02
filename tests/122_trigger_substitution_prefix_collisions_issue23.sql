-- Regression test for issue #23: token-aware NEW/OLD substitution must avoid
-- prefix collisions (e.g. NEW.a must not match NEW.aa) and must not rewrite
-- inside string literals.

DROP TRIGGER IF EXISTS trg_before_issue23 ON t_issue23;
DROP TRIGGER IF EXISTS trg_after_issue23 ON t_issue23;
DROP TABLE IF EXISTS t_issue23;
DROP TABLE IF EXISTS audit_issue23;
DROP FUNCTION IF EXISTS issue23_before();
DROP FUNCTION IF EXISTS issue23_after();

CREATE TABLE t_issue23 (
    a INT,
    aa INT
);

-- BEFORE trigger path (`src/sql/triggers.rs`).
CREATE OR REPLACE FUNCTION issue23_before()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.a := NEW.aa + 1;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_before_issue23
BEFORE INSERT ON t_issue23
FOR EACH ROW EXECUTE FUNCTION issue23_before();

INSERT INTO t_issue23 (a, aa) VALUES (1, 10);

SELECT a, aa FROM t_issue23;

DROP TRIGGER trg_before_issue23 ON t_issue23;
DROP FUNCTION issue23_before();

-- Async AFTER trigger path (`src/sql/trigger_worker.rs`).
CREATE TABLE audit_issue23 (msg TEXT, aa INT);

CREATE OR REPLACE FUNCTION issue23_after()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  INSERT INTO audit_issue23 (msg, aa) VALUES ('NEW.aa', NEW.aa);
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_after_issue23
AFTER INSERT ON t_issue23
FOR EACH ROW EXECUTE FUNCTION issue23_after();

INSERT INTO t_issue23 (a, aa) VALUES (2, 20);

-- Allow background trigger worker to process the queued event.
SELECT pg_sleep(0.3);

SELECT msg, aa FROM audit_issue23 ORDER BY aa;

DROP TRIGGER trg_after_issue23 ON t_issue23;
DROP FUNCTION issue23_after();
DROP TABLE t_issue23;
DROP TABLE audit_issue23;


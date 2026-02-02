-- Issue #34 regression: trigger enqueue must participate in SAVEPOINT undo logging.
-- Expected: ROLLBACK TO SAVEPOINT removes any queued AFTER-trigger events.

DROP TRIGGER IF EXISTS user_insert_audit ON users_sp;
DROP TABLE IF EXISTS users_sp;
DROP TABLE IF EXISTS audit_log_sp;
DROP FUNCTION IF EXISTS log_user_insert_sp();

CREATE TABLE audit_log_sp (id SERIAL PRIMARY KEY, action TEXT);
CREATE TABLE users_sp (id SERIAL PRIMARY KEY, name TEXT);

CREATE OR REPLACE FUNCTION log_user_insert_sp() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO audit_log_sp (action) VALUES ('user_created: ' || NEW.name);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER user_insert_audit
    AFTER INSERT ON users_sp
    FOR EACH ROW EXECUTE FUNCTION log_user_insert_sp();

BEGIN;
SAVEPOINT sp1;
INSERT INTO users_sp (name) VALUES ('Alice');
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

-- Allow background trigger worker to process the queued event (if any).
SELECT pg_sleep(0.3);

SELECT action FROM audit_log_sp ORDER BY id;
SELECT COUNT(*) FROM users_sp;

DROP TRIGGER user_insert_audit ON users_sp;
DROP FUNCTION log_user_insert_sp();
DROP TABLE users_sp;
DROP TABLE audit_log_sp;


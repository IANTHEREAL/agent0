DROP TRIGGER IF EXISTS user_insert_audit ON users;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS audit_log;
DROP FUNCTION IF EXISTS log_user_insert();

CREATE TABLE audit_log (id SERIAL PRIMARY KEY, action TEXT);
CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT);

CREATE OR REPLACE FUNCTION log_user_insert() RETURNS TRIGGER AS $$
BEGIN
    NULL;
    INSERT INTO audit_log (action) VALUES ('user_created: ' || NEW.name);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER user_insert_audit
    AFTER INSERT ON users
    FOR EACH ROW EXECUTE FUNCTION log_user_insert();

INSERT INTO users (name) VALUES ('Alice');

-- Allow background trigger worker to process the queued event.
SELECT pg_sleep(0.3);

SELECT action FROM audit_log ORDER BY id;

DROP TRIGGER user_insert_audit ON users;
DROP FUNCTION log_user_insert();
DROP TABLE users;
DROP TABLE audit_log;

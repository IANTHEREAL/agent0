DROP TABLE IF EXISTS idx_test;

CREATE TABLE idx_test (
    id SERIAL PRIMARY KEY,
    name TEXT,
    status INT,
    email TEXT
);

INSERT INTO idx_test (name, status, email) VALUES ('alice', 1, 'ALICE@example.com');
INSERT INTO idx_test (name, status, email) VALUES ('bob', 0, 'bob@example.com');
INSERT INTO idx_test (name, status, email) VALUES ('charlie', 1, 'Charlie@Example.COM');

CREATE INDEX idx_active ON idx_test (name) WHERE status = 1;

CREATE INDEX idx_lower_email ON idx_test ((lower(email)));

SELECT 'PARTIAL_INDEX=' || count(*) FROM pg_indexes WHERE indexname = 'idx_active';
SELECT 'EXPR_INDEX=' || count(*) FROM pg_indexes WHERE indexname = 'idx_lower_email';

SELECT 'ACTIVE_NAMES=' || string_agg(name, ',') FROM idx_test WHERE status = 1 ORDER BY name;

INSERT INTO idx_test (name, status, email) VALUES ('dave', 1, 'Dave@Example.com');

SELECT 'AFTER_INSERT=' || string_agg(name, ',') FROM idx_test WHERE status = 1 ORDER BY name;

UPDATE idx_test SET status = 0 WHERE name = 'alice';

SELECT 'AFTER_UPDATE=' || string_agg(name, ',') FROM idx_test WHERE status = 1 ORDER BY name;

DROP INDEX idx_active;
DROP INDEX idx_lower_email;
DROP TABLE idx_test;

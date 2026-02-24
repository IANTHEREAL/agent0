-- DDL export table function integration test

-- Cleanup from prior runs
DROP MATERIALIZED VIEW IF EXISTS export_test_mv;
DROP VIEW IF EXISTS export_test_view;
DROP TABLE IF EXISTS export_test_child;
DROP TABLE IF EXISTS export_test_main;
DROP SEQUENCE IF EXISTS export_test_seq;
DROP TYPE IF EXISTS export_test_status;

-- Create test objects in dependency order
CREATE TYPE export_test_status AS ENUM ('active', 'inactive', 'pending');

CREATE SEQUENCE export_test_seq START 100 INCREMENT 5;

CREATE TABLE export_test_main (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    status export_test_status DEFAULT 'active',
    score REAL DEFAULT 0.0,
    created_at TIMESTAMP DEFAULT '2024-01-15 10:00:00',
    CONSTRAINT chk_name CHECK (name <> '')
);

CREATE TABLE export_test_child (
    id SERIAL PRIMARY KEY,
    parent_id INT NOT NULL REFERENCES export_test_main(id) ON DELETE CASCADE,
    data TEXT
);

CREATE INDEX idx_export_test_child_parent ON export_test_child(parent_id);

CREATE VIEW export_test_view AS SELECT id, name, email FROM export_test_main WHERE status = 'active';

CREATE MATERIALIZED VIEW export_test_mv AS SELECT id, name FROM export_test_main;

-- Query DDL export — verify all object types appear
SELECT object_type, object_name FROM _db9_sys_export_ddl() ORDER BY object_type, object_name;

-- Verify specific DDL content for the main table
SELECT ddl_sql FROM _db9_sys_export_ddl() WHERE object_name = 'public.export_test_main' AND object_type = 'table';

-- Cleanup
DROP MATERIALIZED VIEW export_test_mv;
DROP VIEW export_test_view;
DROP TABLE export_test_child;
DROP TABLE export_test_main;
DROP SEQUENCE export_test_seq;
DROP TYPE export_test_status;

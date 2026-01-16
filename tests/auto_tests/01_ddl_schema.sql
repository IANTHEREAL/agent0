-- Auto tests: DDL / Schema
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP VIEW IF EXISTS ddl_basic_view;
DROP TABLE IF EXISTS ddl_basic;
DROP SEQUENCE IF EXISTS ddl_seq;

CREATE TABLE ddl_basic (
    id INT PRIMARY KEY,
    full_name TEXT NOT NULL,
    score INT DEFAULT 10,
    created_at TIMESTAMP DEFAULT '2020-01-01 00:00:00'
);

ALTER TABLE ddl_basic ADD COLUMN active BOOLEAN DEFAULT true;
ALTER TABLE ddl_basic RENAME COLUMN full_name TO name;
ALTER TABLE ddl_basic DROP COLUMN score;

CREATE INDEX idx_ddl_basic_name ON ddl_basic(name);
DROP INDEX idx_ddl_basic_name;

CREATE VIEW ddl_basic_view AS
    SELECT id, name, active FROM ddl_basic;
DROP VIEW ddl_basic_view;

CREATE SEQUENCE ddl_seq START WITH 1 INCREMENT BY 1;
SELECT nextval('ddl_seq');
SELECT currval('ddl_seq');
DROP SEQUENCE ddl_seq;

DROP TABLE ddl_basic;

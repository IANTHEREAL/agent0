-- Auto tests: information_schema / pg_catalog
CREATE SCHEMA IF NOT EXISTS auto_tests;
SET search_path TO auto_tests, public;

DROP TABLE IF EXISTS catalog_demo;

CREATE TABLE catalog_demo (
    id INT PRIMARY KEY,
    name TEXT
);

SELECT table_schema, table_name
FROM information_schema.tables
WHERE table_schema = 'auto_tests' AND table_name = 'catalog_demo';

SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'auto_tests' AND table_name = 'catalog_demo'
ORDER BY ordinal_position;

SELECT relname
FROM pg_catalog.pg_class
WHERE relname = 'catalog_demo';

DROP TABLE catalog_demo;

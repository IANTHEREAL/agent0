-- Parquet COPY FROM should require INSERT privilege

CREATE EXTENSION IF NOT EXISTS parquet;

CREATE TABLE parquet_perm_test (id INT, name TEXT);

DROP ROLE IF EXISTS parquet_viewer;
CREATE ROLE parquet_viewer LOGIN PASSWORD 'viewer';
GRANT SELECT ON parquet_perm_test TO parquet_viewer;

\setenv PGPASSWORD viewer
\connect postgres parquet_viewer

-- Should fail: no INSERT privilege
COPY parquet_perm_test FROM 'https://example.com/test.parquet' WITH (FORMAT parquet);

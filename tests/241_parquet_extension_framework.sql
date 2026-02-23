-- Parquet extension framework: install, catalog visibility, uninstall

DROP EXTENSION IF EXISTS parquet;
CREATE EXTENSION parquet;
CREATE EXTENSION IF NOT EXISTS parquet;

SELECT extname, extversion FROM pg_extension WHERE extname = 'parquet';

-- Verify read_parquet is not in pg_proc (it is a built-in table function, not a UDF)
SELECT COUNT(*) FROM pg_proc WHERE proname = 'read_parquet';

DROP EXTENSION parquet;

-- After drop, read_parquet should fail with extension not installed
SELECT * FROM read_parquet('https://example.com/test.parquet');

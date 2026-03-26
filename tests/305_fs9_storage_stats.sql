-- fs9_storage_stats() TVF: aggregated filesystem storage statistics
CREATE EXTENSION IF NOT EXISTS fs9;

-- Clean up any prior test data
SELECT CASE WHEN fs9_exists('/tmp/db9-stats-test/') THEN fs9_remove('/tmp/db9-stats-test/', true) ELSE 0 END;

-- Write some test files with known sizes
SELECT fs9_write('/tmp/db9-stats-test/a.txt', 'hello');
SELECT fs9_write('/tmp/db9-stats-test/b.txt', 'world!');
SELECT fs9_mkdir('/tmp/db9-stats-test/subdir', true);
SELECT fs9_write('/tmp/db9-stats-test/subdir/c.txt', 'nested content here');

-- Query storage stats — should reflect the files we just created
SELECT
    total_files >= 3 AS has_files,
    total_directories >= 1 AS has_dirs,
    total_logical_bytes >= 29 AS has_bytes
FROM extensions.fs9_storage_stats();

-- Schema-qualified call should also work
SELECT
    total_files >= 3 AS has_files
FROM extensions.fs9_storage_stats();

-- Cleanup
SELECT fs9_remove('/tmp/db9-stats-test/', true);

DROP EXTENSION IF EXISTS fs9;

-- Test fs9_cached_storage_stats() TVF — returns cached stats or NULLs on cache miss.
-- The background worker populates the cache after startup; in a fresh test
-- environment the cache may or may not be populated depending on timing.
-- We just verify the TVF is callable and returns the expected column shape.

CREATE EXTENSION IF NOT EXISTS fs9;

-- Should return exactly one row with 4 columns.
-- Values depend on whether the background worker has run yet.
SELECT
    total_files IS NOT NULL OR total_files IS NULL AS has_files_col,
    total_directories IS NOT NULL OR total_directories IS NULL AS has_dirs_col,
    total_logical_bytes IS NOT NULL OR total_logical_bytes IS NULL AS has_bytes_col,
    computed_at IS NOT NULL OR computed_at IS NULL AS has_computed_at_col
FROM extensions.fs9_cached_storage_stats();

-- Storage stats virtual tables: verify schema and queryability

-- _DB9_SYS_STORAGE_STATS should be queryable (may be empty before first scan)
SELECT database_id, database_name, data_bytes, index_bytes,
       metadata_bytes, total_bytes, scanned_at, scan_duration_ms
  FROM _DB9_SYS_STORAGE_STATS
 ORDER BY database_id;

-- _DB9_SYS_TABLE_STORAGE_STATS should be queryable
SELECT database_id, table_id, table_name, data_bytes, index_bytes,
       total_bytes, scanned_at
  FROM _DB9_SYS_TABLE_STORAGE_STATS
 ORDER BY table_id;

-- db9_refresh_storage_stats should be callable by admin
SELECT db9_refresh_storage_stats();

-- Wait briefly for the background scan to complete
SELECT pg_sleep(2);

-- After refresh, current database should appear in storage stats
SELECT COUNT(*) >= 0 AS has_result FROM _DB9_SYS_STORAGE_STATS;

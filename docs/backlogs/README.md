# Design Documents

Remaining design docs for features not yet implemented.

## Remaining

| Doc | Feature | Priority | Notes |
|-----|---------|----------|-------|
| [06_copy_to_and_options.md](06_copy_to_and_options.md) | COPY TO stdout | P1 | Enables pg_dump |
| [11_system_catalog_coverage.md](11_system_catalog_coverage.md) | pg_catalog coverage | P0 | Ongoing |
| [13_listen_notify.md](13_listen_notify.md) | LISTEN/NOTIFY | P3 | Tricky for stateless arch |
| [17_extensions_framework_http.md](17_extensions_framework_http.md) | Extensions + HTTP | P2 | Supabase-style |
| [18_async_trigger_queue.md](18_async_trigger_queue.md) | Async AFTER triggers | P2 | Queue-based, multi-tenant |

## Implemented (removed)

The following design docs were removed as their features are now implemented:

- Savepoints (SAVEPOINT/ROLLBACK TO/RELEASE)
- ALTER TABLE migration subset
- User-defined types (CREATE TYPE AS ENUM)
- Sequences (CREATE SEQUENCE, nextval, SERIAL)
- Schemas & search_path
- Dollar-quoted strings
- NUMERIC/DECIMAL type
- DATE type
- pgwire Array/Vector OIDs
- Functions & Triggers DDL
- Partial/Expression indexes
- EXPLAIN ANALYZE
- Set-returning functions (generate_series, unnest)

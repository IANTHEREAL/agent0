-- Migrations table function schema validation
SELECT name, applied_at, checksum, sql_preview FROM _pgtikv_sys_migrations() ORDER BY name;

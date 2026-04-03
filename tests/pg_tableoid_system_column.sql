-- Test: tableoid system column on catalog tables (pg_dump compatibility)
-- pg_dump SELECTs tableoid from nearly every catalog query.

-- pg_namespace.tableoid = 2615 (always has 'public' schema)
SELECT n.tableoid FROM pg_namespace n WHERE n.nspname = 'public';

-- pg_class.tableoid = 1259 (always has at least one entry)
SELECT DISTINCT c.tableoid FROM pg_class c LIMIT 1;

-- pg_dump's exact pg_extension query shape (may return 0 rows, must not error)
SELECT x.tableoid, x.oid, x.extname, n.nspname, x.extrelocatable, x.extversion, x.extconfig, x.extcondition
FROM pg_extension x
JOIN pg_namespace n ON n.oid = x.extnamespace;

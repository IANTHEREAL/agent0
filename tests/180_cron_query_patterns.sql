-- pg_cron advanced query patterns on virtual tables

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Setup: diverse jobs
SELECT cron.schedule('backup_daily', '0 2 * * *', 'SELECT pg_dump()');
SELECT cron.schedule('vacuum_hourly', '0 * * * *', 'VACUUM');
SELECT cron.schedule('refresh_mv', '*/15 * * * *', 'REFRESH MATERIALIZED VIEW mv1');
SELECT cron.schedule('cleanup_logs', '0 3 * * *', 'DELETE FROM logs');
SELECT cron.schedule('stats_update', '*/30 * * * *', 'ANALYZE');

-- Disable some
SELECT cron.alter_job(2, NULL, NULL, NULL, NULL, false);
SELECT cron.alter_job(4, NULL, NULL, NULL, NULL, false);

-- WHERE with AND
SELECT jobname FROM cron.job WHERE active = true AND schedule LIKE '%*/%' ORDER BY jobname;

-- WHERE with OR
SELECT jobname FROM cron.job WHERE jobname = 'backup_daily' OR jobname = 'vacuum_hourly' ORDER BY jobname;

-- WHERE with IN
SELECT jobname FROM cron.job WHERE jobid IN (1, 3, 5) ORDER BY jobname;

-- WHERE with LIKE pattern
SELECT jobname FROM cron.job WHERE command LIKE '%VACUUM%' OR command LIKE '%ANALYZE%' ORDER BY jobname;

-- WHERE with NOT
SELECT jobname FROM cron.job WHERE active = true AND jobname NOT LIKE '%backup%' ORDER BY jobname;

-- ORDER BY different columns
SELECT jobname, schedule FROM cron.job ORDER BY schedule;
SELECT jobname FROM cron.job ORDER BY jobname DESC;

-- LIMIT and OFFSET
SELECT jobname FROM cron.job ORDER BY jobid LIMIT 3;
SELECT jobname FROM cron.job ORDER BY jobid LIMIT 2 OFFSET 2;

-- Aggregate: count by active status
SELECT active, count(*) as cnt FROM cron.job GROUP BY active ORDER BY active;

-- Subquery: jobs whose schedule matches a specific pattern
SELECT jobname FROM cron.job WHERE jobid IN (
    SELECT jobid FROM cron.job WHERE schedule LIKE '0 % * * *'
) ORDER BY jobname;

-- Cleanup
SELECT cron.unschedule('backup_daily');
SELECT cron.unschedule('vacuum_hourly');
SELECT cron.unschedule('refresh_mv');
SELECT cron.unschedule('cleanup_logs');
SELECT cron.unschedule('stats_update');
DROP EXTENSION pg_cron;

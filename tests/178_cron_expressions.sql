-- pg_cron expression parsing edge cases

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Valid: step values
SELECT cron.schedule('step_min', '*/10 * * * *', 'SELECT 1');
SELECT cron.schedule('step_hour', '0 */3 * * *', 'SELECT 1');

-- Valid: ranges
SELECT cron.schedule('range_wkday', '0 9 * * 1-5', 'SELECT 1');
SELECT cron.schedule('range_hour', '30 8-18 * * *', 'SELECT 1');

-- Valid: lists
SELECT cron.schedule('list_min', '0,15,30,45 * * * *', 'SELECT 1');
SELECT cron.schedule('list_month', '0 0 1 1,4,7,10 *', 'SELECT 1');

-- Valid: combined range+step
SELECT cron.schedule('range_step', '0 9-17/2 * * *', 'SELECT 1');

-- Valid: specific day of month
SELECT cron.schedule('monthly_1st', '0 0 1 * *', 'SELECT 1');
SELECT cron.schedule('monthly_15th', '0 12 15 * *', 'SELECT 1');

-- Valid: last day semantics with wildcards
SELECT cron.schedule('daily_midnight', '0 0 * * *', 'SELECT 1');

SELECT count(*) FROM cron.job;

-- Invalid: @daily shorthand
SELECT cron.schedule('@daily', 'SELECT 1');

-- Invalid: @hourly shorthand
SELECT cron.schedule('@hourly', 'SELECT 1');

-- Invalid: @reboot
SELECT cron.schedule('@reboot', 'SELECT 1');

-- Invalid: @yearly
SELECT cron.schedule('@yearly', 'SELECT 1');

-- Invalid: interval syntax (30 seconds)
SELECT cron.schedule('30 seconds', 'SELECT 1');

-- Invalid: interval syntax (5 minutes)
SELECT cron.schedule('5 minutes', 'SELECT 1');

-- Invalid: too few fields (3)
SELECT cron.schedule('0 12 *', 'SELECT 1');

-- Invalid: too many fields (7)
SELECT cron.schedule('0 0 12 * * * *', 'SELECT 1');

-- Invalid: whitespace only
SELECT cron.schedule('   ', 'SELECT 1');

-- Cleanup
SELECT cron.unschedule('step_min');
SELECT cron.unschedule('step_hour');
SELECT cron.unschedule('range_wkday');
SELECT cron.unschedule('range_hour');
SELECT cron.unschedule('list_min');
SELECT cron.unschedule('list_month');
SELECT cron.unschedule('range_step');
SELECT cron.unschedule('monthly_1st');
SELECT cron.unschedule('monthly_15th');
SELECT cron.unschedule('daily_midnight');
DROP EXTENSION pg_cron;

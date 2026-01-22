-- statement_timeout enforcement (cooperative via per-statement timeout wrapper)

SET statement_timeout = 50;
SELECT pg_sleep(0.2);
SELECT 1 AS after_timeout;

SET statement_timeout = 0;
SELECT pg_sleep(0.05);
SELECT 1 AS after_disabled;


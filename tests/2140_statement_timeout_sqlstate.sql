-- Regression: statement_timeout must emit SQLSTATE 57014 (query_canceled).
-- PostgreSQL parity verified on PG 17.9.
\set VERBOSITY verbose

SET statement_timeout = '200ms';
SELECT pg_sleep(10);
SET statement_timeout = '0';
SELECT 1 AS after_timeout;

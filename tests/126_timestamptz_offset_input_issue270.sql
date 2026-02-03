-- Regression test for issue #270:
-- Accept Postgres-style TIMESTAMPTZ offsets with colon (e.g. +00:00) which are common in ORMs.

SET TIME ZONE 'UTC';

SELECT 'OK' WHERE '2024-01-15 10:00:00.123 +00:00'::timestamptz IS NOT NULL;
SELECT 'OK' WHERE '2024-01-15 10:00:00 +00:00'::timestamptz IS NOT NULL;
SELECT 'OK' WHERE '2024-01-15 10:00:00 -05:00'::timestamptz IS NOT NULL;


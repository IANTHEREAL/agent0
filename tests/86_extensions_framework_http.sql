-- Extensions framework + pg_catalog visibility for built-in http extension

DROP EXTENSION IF EXISTS http;
CREATE EXTENSION http;
CREATE EXTENSION IF NOT EXISTS http;

SELECT extname, extversion FROM pg_extension WHERE extname = 'http' ORDER BY extname;

SELECT proname FROM pg_proc WHERE proname LIKE 'http_%' ORDER BY proname;


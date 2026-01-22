-- PostgreSQL compatible tests from contention_event
-- 13 tests

-- NOTE: The upstream CockroachDB test for contention events relies on:
-- - multi-session / multi-node directives (e.g. "user ... nodeidx=...")
-- - crdb_internal tracing tables and payload decoding
-- PostgreSQL does not provide equivalents for that telemetry. Below is a
-- PostgreSQL-native approximation that validates we can observe lock
-- contention via pg_stat_activity.

SET client_min_messages = warning;

DROP TABLE IF EXISTS kv;
CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT);
INSERT INTO kv VALUES ('k', 'v');

-- Use current connection details for the background psql process.
SELECT current_database() AS dbname, current_user AS dbuser \gset
\setenv PGDATABASE :dbname
\setenv PGUSER :dbuser
\setenv PGHOST /var/run/postgresql
\setenv PGPORT 5432
\setenv PIDFILE /tmp/pg_tests_contention_event.pid
\setenv OUTFILE /tmp/pg_tests_contention_event.out

BEGIN;
UPDATE kv SET v = 'v1' WHERE k = 'k';

-- Start a second session that will block on the same row lock.
\! sh -c "rm -f $PIDFILE"
\! sh -c "rm -f $OUTFILE"
\! sh -c "psql -X -v ON_ERROR_STOP=1 -c \"SET application_name = 'contention_event_bg'; BEGIN; UPDATE kv SET v = 'v2' WHERE k = 'k'; COMMIT;\" >$OUTFILE 2>&1 & echo $! > $PIDFILE"

SELECT pg_sleep(0.5);

-- Confirm there is at least one backend waiting on a lock.
SELECT count(*) > 0
FROM pg_stat_activity
WHERE application_name = 'contention_event_bg'
  AND wait_event_type = 'Lock';

COMMIT;

-- Wait for the background process to finish after the lock is released.
\! sh -c "while [ -f $PIDFILE ] && kill -0 $(cat $PIDFILE) 2>/dev/null; do sleep 0.05; done"
\! sh -c "cat $OUTFILE"
\! sh -c "rm -f $PIDFILE"
\! sh -c "rm -f $OUTFILE"

SELECT * FROM kv ORDER BY k;

RESET client_min_messages;

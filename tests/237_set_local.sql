-- SET LOCAL transaction scoping, set_config(..., true), and savepoint behavior.
-- Use deterministic values to avoid environment-specific defaults.
\pset tuples_only on

RESET timezone;
SET application_name = '';

-- 1) SET LOCAL reverts at COMMIT
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SHOW timezone;
COMMIT;
SHOW timezone;

-- 2) SET LOCAL reverts at ROLLBACK
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SHOW timezone;
ROLLBACK;
SHOW timezone;

-- 3) SAVEPOINT + ROLLBACK TO restores prior local value
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SAVEPOINT sp1;
SET LOCAL timezone = 'America/New_York';
SHOW timezone;
ROLLBACK TO sp1;
SHOW timezone;
COMMIT;
SHOW timezone;

-- 4) SET LOCAL outside transaction: warning + no effect
SET timezone = 'UTC';
SET LOCAL timezone = 'Asia/Shanghai';
SHOW timezone;

-- 5) RESET clears local override in current transaction
SET application_name = '';
BEGIN;
SET LOCAL application_name = 'local_app';
SET application_name = '';
SHOW application_name;
COMMIT;
SHOW application_name;

-- 6) RESET ALL clears local overrides
RESET standard_conforming_strings;
BEGIN;
SET LOCAL standard_conforming_strings = off;
RESET ALL;
SHOW standard_conforming_strings;
COMMIT;
SHOW standard_conforming_strings;

-- 7) SET then SET LOCAL: local in txn, SET value after COMMIT
SET timezone = 'Asia/Shanghai';
BEGIN;
SET LOCAL timezone = 'UTC';
SHOW timezone;
COMMIT;
SHOW timezone;
RESET timezone;

-- 8) SET LOCAL then regular SET: regular SET wins and persists
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SET timezone = 'UTC';
SHOW timezone;
COMMIT;
SHOW timezone;

-- 9) set_config(..., true) inside transaction is local-only
SET timezone = 'UTC';
BEGIN;
SELECT set_config('timezone', 'Asia/Shanghai', true) AS v;
SHOW timezone;
COMMIT;
SHOW timezone;

-- 10) set_config(..., true) outside transaction returns new value but does not persist
SET timezone = 'UTC';
SELECT set_config('timezone', 'Asia/Shanghai', true) AS v;
SHOW timezone;

-- 11) RELEASE SAVEPOINT does not cancel earlier SET LOCAL
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SAVEPOINT sp1;
RELEASE SAVEPOINT sp1;
SHOW timezone;
COMMIT;
SHOW timezone;

-- 12) Nested savepoints with SET LOCAL at each level
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SAVEPOINT s1;
SET LOCAL timezone = 'America/New_York';
SAVEPOINT s2;
SET LOCAL timezone = 'Europe/London';
SHOW timezone;
ROLLBACK TO s2;
SHOW timezone;
ROLLBACK TO s1;
SHOW timezone;
COMMIT;
SHOW timezone;

-- 13) RESET ALL inside savepoint, then ROLLBACK TO restores local snapshots
RESET standard_conforming_strings;
BEGIN;
SET LOCAL standard_conforming_strings = off;
SAVEPOINT rs;
RESET ALL;
SHOW standard_conforming_strings;
ROLLBACK TO rs;
SHOW standard_conforming_strings;
COMMIT;
SHOW standard_conforming_strings;

-- 14) SET LOCAL search_path inside transaction reverts at COMMIT
SET search_path = public;
BEGIN;
SET LOCAL search_path TO pg_catalog, public;
SHOW search_path;
COMMIT;
SHOW search_path;

-- 15) SET LOCAL search_path outside transaction warns + no effect
SET search_path = public;
SET LOCAL search_path TO pg_catalog, public;
SHOW search_path;

RESET timezone;
SET application_name = '';

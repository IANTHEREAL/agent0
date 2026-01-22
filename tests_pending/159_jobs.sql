-- PostgreSQL compatible tests from jobs
-- 54 tests

SET client_min_messages = warning;

-- CockroachDB-only settings (no-op on PostgreSQL).
-- SET CLUSTER SETTING sql.defaults.use_declarative_schema_changer = 'off';
-- SET CLUSTER SETTING sql.defaults.create_table_with_schema_locked = 'off';
-- SET use_declarative_schema_changer = 'off';
-- SET create_table_with_schema_locked = 'off';

-- Basic role + DDL smoke test. CockroachDB's background job system and
-- job-control statements (crdb_internal.jobs, PAUSE/CANCEL/RESUME JOB,
-- CONTROLJOB/VIEWJOB) do not exist in PostgreSQL.

DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
DROP ROLE IF EXISTS jobviewer;
DROP ROLE IF EXISTS jobcontroller;

CREATE ROLE testuser LOGIN;
CREATE ROLE testuser2 LOGIN;

-- Test 4: statement (line 20)
-- The source test grants privileges on database 'test'. Use the current DB.
SELECT format('GRANT ALL ON DATABASE %I TO testuser', current_database())
\gexec

-- Test 5-7: schema changes.
CREATE TABLE t(x INT);
INSERT INTO t(x) VALUES (1);
CREATE INDEX ON t(x);

-- Tests 8-14: crdb_internal.jobs is not available in PostgreSQL.
-- SELECT job_type, description, user_name FROM crdb_internal.jobs ...

-- Test 11-12: more schema changes.
CREATE TABLE u(x INT);
INSERT INTO u(x) VALUES (1);
CREATE INDEX ON u(x);

-- Test 15-21: a separate role performs schema changes.
SELECT format('GRANT CREATE ON DATABASE %I TO testuser2', current_database())
\gexec

DROP SCHEMA IF EXISTS testuser2 CASCADE;
CREATE SCHEMA testuser2 AUTHORIZATION testuser2;

SET ROLE testuser2;
CREATE TABLE t1(x INT);
INSERT INTO t1(x) VALUES (1);
CREATE INDEX ON t1(x);
DROP TABLE t1;
RESET ROLE;

-- Remaining tests are CockroachDB job-control features, unsupported in PG.
-- PAUSE JOB / CANCEL JOB / RESUME JOB ...

-- Cleanup (avoid persistent roles/privileges across test runs).
DROP TABLE IF EXISTS u;
DROP TABLE IF EXISTS t;
DROP SCHEMA IF EXISTS testuser2 CASCADE;

SELECT format('REVOKE ALL ON DATABASE %I FROM testuser', current_database())
\gexec
SELECT format('REVOKE ALL ON DATABASE %I FROM testuser2', current_database())
\gexec

DROP ROLE testuser2;
DROP ROLE testuser;

RESET client_min_messages;

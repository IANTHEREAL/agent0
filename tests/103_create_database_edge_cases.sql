-- CREATE/DROP/ALTER DATABASE edge cases and compatibility behavior.

-- pg_database should exist and include the default database.
SELECT datname FROM pg_catalog.pg_database ORDER BY datname;

-- Idempotent cleanup (expects NOTICEs in a fresh keyspace).
DROP DATABASE IF EXISTS createdb_103_opts;
DROP DATABASE IF EXISTS createdb_103_iso;
DROP DATABASE IF EXISTS createdb_103_current;
DROP DATABASE IF EXISTS createdb_103_owner;
DROP DATABASE IF EXISTS createdb_103_a;
DROP DATABASE IF EXISTS createdb_103_b;

-- Reserved/system database protections.
CREATE DATABASE postgres;
DROP DATABASE postgres;
ALTER DATABASE postgres RENAME TO createdb_103_pg;

-- pg_dump-style CREATE DATABASE options should be accepted (and ignored).
CREATE DATABASE createdb_103_opts WITH TEMPLATE = template0 ENCODING = 'UTF8' LC_COLLATE = 'English_United States.1252' LC_CTYPE = 'English_United States.1252';
SELECT datname FROM pg_catalog.pg_database WHERE datname = 'createdb_103_opts' ORDER BY datname;

-- IF NOT EXISTS should not error and should emit a NOTICE.
CREATE DATABASE IF NOT EXISTS createdb_103_opts;

-- Unsupported CREATE DATABASE options should error deterministically.
CREATE DATABASE createdb_103_badenc ENCODING='LATIN1';
CREATE DATABASE createdb_103_badtemplate TEMPLATE = badtmpl;
CREATE DATABASE createdb_103_badopt WITH CONNECTION LIMIT = 5;
CREATE DATABASE bad-name;

-- RENAME should fail if the target name already exists.
CREATE DATABASE createdb_103_a;
CREATE DATABASE createdb_103_b;
ALTER DATABASE createdb_103_a RENAME TO createdb_103_b;
DROP DATABASE createdb_103_a;
DROP DATABASE createdb_103_b;

-- CREATE/DROP/ALTER DATABASE cannot run inside a transaction block.
BEGIN;
CREATE DATABASE createdb_103_txn;
ROLLBACK;

BEGIN;
DROP DATABASE createdb_103_opts;
ROLLBACK;

BEGIN;
ALTER DATABASE createdb_103_opts RENAME TO createdb_103_opts2;
ROLLBACK;

-- Database-level isolation: tables created in one database must not appear in another.
CREATE DATABASE createdb_103_iso;
\connect createdb_103_iso
CREATE TABLE public.isot (id INT PRIMARY KEY);
SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema='public' AND table_name='isot' ORDER BY table_schema, table_name;
\connect postgres
SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema='public' AND table_name='isot' ORDER BY table_schema, table_name;

-- Cannot drop/rename the currently open database.
CREATE DATABASE createdb_103_current;
\connect createdb_103_current
DROP DATABASE createdb_103_current;
ALTER DATABASE createdb_103_current RENAME TO createdb_103_current2;
\connect postgres
DROP DATABASE createdb_103_current;

-- ALTER DATABASE OWNER TO (pg_dump compatibility; no output, but must succeed).
CREATE DATABASE createdb_103_owner WITH OWNER = admin;
ALTER DATABASE createdb_103_owner OWNER TO admin;
DROP DATABASE createdb_103_owner;

-- Cleanup (IF EXISTS should emit a NOTICE after the database has been dropped).
DROP DATABASE createdb_103_iso;
DROP DATABASE IF EXISTS createdb_103_iso;
DROP DATABASE createdb_103_opts;
DROP DATABASE IF EXISTS createdb_103_opts;

-- Ensure we cleaned up all test databases.
SELECT datname FROM pg_catalog.pg_database WHERE datname LIKE 'createdb_103_%' ORDER BY datname;

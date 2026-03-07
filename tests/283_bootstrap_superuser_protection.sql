-- Bootstrap superuser protections (#1439).
\set VERBOSITY verbose

DROP ROLE IF EXISTS bootstrap_renamer_283;
CREATE ROLE bootstrap_renamer_283 LOGIN SUPERUSER PASSWORD 'bootstrap_renamer_283';

SELECT rolname, rolsuper FROM pg_catalog.pg_roles WHERE rolname = 'admin';

\setenv PGPASSWORD bootstrap_renamer_283
\connect postgres bootstrap_renamer_283

ALTER ROLE admin RENAME TO admin2;
ALTER ROLE admin2 WITH NOSUPERUSER;
DROP ROLE admin2;

\setenv PGPASSWORD admin
\connect postgres admin2

ALTER ROLE admin2 RENAME TO admin3;
DROP ROLE admin2;

SELECT rolname, rolsuper FROM pg_catalog.pg_roles WHERE rolname = 'admin2';

\setenv PGPASSWORD bootstrap_renamer_283
\connect postgres bootstrap_renamer_283
ALTER ROLE admin2 RENAME TO admin;

DROP ROLE bootstrap_renamer_283;

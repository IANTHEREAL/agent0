-- Issue #1475: PostgreSQL-compatible USER/ROLE DDL aliases + pg_user catalog view.

DROP ROLE IF EXISTS issue1475_readonly;

CREATE USER issue1475_readonly WITH PASSWORD 'test123';

SELECT 'a1_pg_user_has_readonly=' ||
       (EXISTS (
           SELECT 1
           FROM pg_catalog.pg_user u
           WHERE u.usename = 'issue1475_readonly'
       ))::text;

SELECT 'a2_pg_user_row=' ||
       (SELECT u.usename
        FROM pg_catalog.pg_user u
        WHERE u.usename = 'issue1475_readonly');

DROP USER issue1475_readonly;

SELECT 'a3_pg_user_dropped=' ||
       (NOT EXISTS (
           SELECT 1
           FROM pg_catalog.pg_user u
           WHERE u.usename = 'issue1475_readonly'
       ))::text;

-- fs9 SECURITY DEFINER permission semantics regression test.
-- Verifies that SECURITY DEFINER functions owned by a superuser can call
-- fs9 functions, while direct calls and SECURITY INVOKER wrappers cannot.
CREATE EXTENSION IF NOT EXISTS fs9;

-- Setup: write a test file as superuser
SELECT fs9_write('/test_secdef_fs9.txt', 'hello from secdef test') AS setup_write;

-- Setup: create a non-superuser role
DROP ROLE IF EXISTS fs9_secdef_caller;
CREATE ROLE fs9_secdef_caller LOGIN PASSWORD 'pw';

-- Q1: Direct call as non-superuser — must be denied
SET ROLE fs9_secdef_caller;
SELECT fs9_read_bytea('/test_secdef_fs9.txt');
RESET ROLE;

-- Q2: SECURITY DEFINER wrapper owned by superuser (admin) — must succeed
-- The wrapper runs with admin's privileges, so fs9 permission check passes.
-- Functions have EXECUTE granted to PUBLIC by default.
CREATE OR REPLACE FUNCTION secdef_fs9_read(p TEXT)
RETURNS BYTEA
LANGUAGE SQL
SECURITY DEFINER
AS $$ SELECT fs9_read_bytea(p) $$;

SET ROLE fs9_secdef_caller;
SELECT length(secdef_fs9_read('/test_secdef_fs9.txt')) AS secdef_read_len;
RESET ROLE;

-- Q3: SECURITY INVOKER wrapper — must be denied (runs as caller, not owner)
CREATE OR REPLACE FUNCTION invoker_fs9_read(p TEXT)
RETURNS BYTEA
LANGUAGE SQL
SECURITY INVOKER
AS $$ SELECT fs9_read_bytea(p) $$;

SET ROLE fs9_secdef_caller;
SELECT invoker_fs9_read('/test_secdef_fs9.txt');
RESET ROLE;

-- Q4: PL/pgSQL SECURITY DEFINER wrapper — must also succeed
CREATE OR REPLACE FUNCTION secdef_fs9_read_plpgsql(p TEXT)
RETURNS BYTEA
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
BEGIN
    RETURN fs9_read_bytea(p);
END;
$$;

SET ROLE fs9_secdef_caller;
SELECT length(secdef_fs9_read_plpgsql('/test_secdef_fs9.txt')) AS secdef_plpgsql_read_len;
RESET ROLE;

-- Cleanup
DROP FUNCTION IF EXISTS secdef_fs9_read(TEXT);
DROP FUNCTION IF EXISTS invoker_fs9_read(TEXT);
DROP FUNCTION IF EXISTS secdef_fs9_read_plpgsql(TEXT);
DROP ROLE IF EXISTS fs9_secdef_caller;
SELECT fs9_remove('/test_secdef_fs9.txt') AS cleanup;

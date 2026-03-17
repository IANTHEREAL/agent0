-- RLS SECURITY DEFINER Integration Tests
-- Purpose: Verify that SECURITY DEFINER functions execute with the
-- owner's identity for RLS policy evaluation, and that pg_proc.prosecdef
-- correctly reflects the attribute.

-- Setup: create roles and table
DROP TABLE IF EXISTS secdef_data CASCADE;
DROP FUNCTION IF EXISTS secdef_read_all();
DROP FUNCTION IF EXISTS invoker_read_all();
DROP ROLE IF EXISTS secdef_owner;
DROP ROLE IF EXISTS secdef_caller;
CREATE ROLE secdef_owner LOGIN PASSWORD 'pw';
CREATE ROLE secdef_caller LOGIN PASSWORD 'pw';
GRANT secdef_owner TO CURRENT_USER WITH SET TRUE;
GRANT secdef_caller TO CURRENT_USER WITH SET TRUE;
GRANT CREATE ON SCHEMA public TO secdef_owner;

CREATE TABLE secdef_data (
    id INT PRIMARY KEY,
    owner_name TEXT NOT NULL,
    secret TEXT NOT NULL
);

INSERT INTO secdef_data VALUES
    (1, 'secdef_owner', 'owner-secret-1'),
    (2, 'secdef_owner', 'owner-secret-2'),
    (3, 'secdef_caller', 'caller-secret-1'),
    (4, 'other', 'other-secret');

-- Grant SELECT to both roles
GRANT SELECT ON secdef_data TO secdef_owner, secdef_caller;

-- Enable RLS: users can only see their own rows
ALTER TABLE secdef_data ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_rows ON secdef_data
    FOR SELECT
    USING (owner_name = current_user);

-- Q1: Verify RLS works — caller sees only their own row
SET ROLE secdef_caller;
SELECT id, secret FROM secdef_data ORDER BY id;
RESET ROLE;

-- Q2: Owner sees their own rows (owner bypass unless FORCE)
SET ROLE secdef_owner;
SELECT id, secret FROM secdef_data ORDER BY id;
RESET ROLE;

-- Create a SECURITY DEFINER function owned by secdef_owner.
-- When secdef_caller calls this, RLS should evaluate using secdef_owner's identity.
SET ROLE secdef_owner;
CREATE FUNCTION secdef_read_all()
RETURNS SETOF secdef_data
LANGUAGE SQL
SECURITY DEFINER
AS $$ SELECT * FROM secdef_data ORDER BY id $$;
RESET ROLE;

-- Grant EXECUTE to caller
GRANT EXECUTE ON FUNCTION secdef_read_all() TO secdef_caller;

-- Create a normal (SECURITY INVOKER) function for comparison
SET ROLE secdef_owner;
CREATE FUNCTION invoker_read_all()
RETURNS SETOF secdef_data
LANGUAGE SQL
AS $$ SELECT * FROM secdef_data ORDER BY id $$;
RESET ROLE;

GRANT EXECUTE ON FUNCTION invoker_read_all() TO secdef_caller;

-- Q3: SECURITY DEFINER — caller sees owner's rows (RLS uses owner identity)
SET ROLE secdef_caller;
SELECT id, secret FROM secdef_read_all();
RESET ROLE;

-- Q4: SECURITY INVOKER — caller sees only their own rows
SET ROLE secdef_caller;
SELECT id, secret FROM invoker_read_all();
RESET ROLE;

-- Q5: Verify pg_proc.prosecdef reflects the attribute correctly
SELECT proname, prosecdef
FROM pg_proc
WHERE proname IN ('secdef_read_all', 'invoker_read_all')
ORDER BY proname;

-- Cleanup
DROP FUNCTION IF EXISTS secdef_read_all();
DROP FUNCTION IF EXISTS invoker_read_all();
DROP TABLE IF EXISTS secdef_data CASCADE;
REVOKE CREATE ON SCHEMA public FROM secdef_owner;
REVOKE secdef_owner FROM CURRENT_USER;
REVOKE secdef_caller FROM CURRENT_USER;
DROP ROLE IF EXISTS secdef_owner;
DROP ROLE IF EXISTS secdef_caller;

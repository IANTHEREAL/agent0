-- PostgreSQL compatible tests from show_grants_synthetic
-- 16 tests

SET client_min_messages = warning;

-- CockroachDB "SYSTEM" privileges don't exist in PostgreSQL. Model them as
-- role memberships and expose them via a helper function.
RESET ROLE;
DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
DROP ROLE IF EXISTS repairclustermetadata;
DROP ROLE IF EXISTS repaircluster;
DROP ROLE IF EXISTS externalconnection;
DROP ROLE IF EXISTS viewactivity;
DROP ROLE IF EXISTS modifyclustersetting;

CREATE ROLE testuser LOGIN;
CREATE ROLE modifyclustersetting;
CREATE ROLE viewactivity;
CREATE ROLE externalconnection;
CREATE ROLE repaircluster;
CREATE ROLE repairclustermetadata;

CREATE OR REPLACE FUNCTION public.pg_show_system_grants()
RETURNS TABLE (grantee TEXT, privilege TEXT, grantor TEXT, grantable BOOL)
LANGUAGE sql
AS $$
SELECT
  grantee.rolname::text AS grantee,
  granted.rolname::text AS privilege,
  grantor.rolname::text AS grantor,
  m.admin_option AS grantable
FROM pg_auth_members AS m
JOIN pg_roles AS grantee ON grantee.oid = m.member
JOIN pg_roles AS granted ON granted.oid = m.roleid
JOIN pg_roles AS grantor ON grantor.oid = m.grantor
WHERE granted.rolname IN (
  'modifyclustersetting',
  'viewactivity',
  'externalconnection',
  'repaircluster',
  'repairclustermetadata'
)
ORDER BY grantee, privilege;
$$;

-- Test 1: statement (line 1)
GRANT modifyclustersetting TO testuser;

-- Test 2: statement (line 4)
GRANT viewactivity TO testuser;

-- Test 3: statement (line 7)
GRANT externalconnection TO testuser WITH ADMIN OPTION;

-- Test 4: query (line 10)
SELECT * FROM public.pg_show_system_grants();

-- Test 5: query (line 18)
SELECT * FROM public.pg_show_system_grants() WHERE grantee IN ('testuser');

-- Test 6: statement (line 26)
REVOKE viewactivity FROM testuser;

-- Test 7: query (line 29)
SELECT * FROM public.pg_show_system_grants();

-- Test 8: statement (line 36)
CREATE USER testuser2;

-- Test 9: statement (line 39)
GRANT viewactivity TO testuser2 WITH ADMIN OPTION;

-- Test 10: statement (line 42)
GRANT externalconnection TO testuser2;

-- Test 11: statement (line 46)
GRANT repaircluster TO testuser2;

-- Test 12: statement (line 49)
GRANT repairclustermetadata TO testuser;

-- Test 13: query (line 52)
SELECT * FROM public.pg_show_system_grants();

-- Test 14: query (line 63)
SELECT * FROM public.pg_show_system_grants() WHERE grantee IN ('testuser2');

-- Test 15: query (line 71)
SELECT * FROM public.pg_show_system_grants() WHERE grantee IN ('testuser', 'testuser2');

-- Test 16: statement (line 87)
SELECT * FROM public.pg_show_system_grants() WHERE grantee IN ('testuser');

RESET client_min_messages;

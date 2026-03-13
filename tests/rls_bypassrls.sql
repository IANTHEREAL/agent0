-- RLS BYPASSRLS Attribute Tests
-- Purpose: Verify that BYPASSRLS role attribute allows bypassing RLS policies.
-- Covers: CREATE ROLE BYPASSRLS, ALTER ROLE BYPASSRLS/NOBYPASSRLS,
--         pg_roles.rolbypassrls reflection, SELECT/DML bypass behavior.

-- Setup: create table and roles
DROP TABLE IF EXISTS rls_bypass_test CASCADE;
DROP ROLE IF EXISTS rls_bypass_user;
DROP ROLE IF EXISTS rls_normal_user;

CREATE ROLE rls_bypass_user LOGIN PASSWORD 'pw' BYPASSRLS;
CREATE ROLE rls_normal_user LOGIN PASSWORD 'pw';

CREATE TABLE rls_bypass_test (
    id INT PRIMARY KEY,
    owner_name TEXT NOT NULL,
    data TEXT NOT NULL
);

INSERT INTO rls_bypass_test VALUES
    (1, 'alice', 'Alice data'),
    (2, 'bob',   'Bob data'),
    (3, 'carol', 'Carol data');

GRANT SELECT ON rls_bypass_test TO rls_bypass_user, rls_normal_user;

-- 1. Verify pg_roles reflects BYPASSRLS attribute
SELECT rolname, rolbypassrls FROM pg_roles
  WHERE rolname IN ('rls_bypass_user', 'rls_normal_user')
  ORDER BY rolname;

-- 2. Enable RLS with a restrictive policy
ALTER TABLE rls_bypass_test ENABLE ROW LEVEL SECURITY;

CREATE POLICY see_own ON rls_bypass_test
    FOR SELECT
    USING (owner_name = current_user);

-- 3. Normal user only sees their own rows (none, since they're not alice/bob/carol)
SET ROLE rls_normal_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- 4. BYPASSRLS user sees all rows despite RLS
SET ROLE rls_bypass_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- 5. ALTER ROLE to remove BYPASSRLS
ALTER ROLE rls_bypass_user NOBYPASSRLS;

SELECT rolname, rolbypassrls FROM pg_roles WHERE rolname = 'rls_bypass_user';

-- 6. After NOBYPASSRLS, user is subject to RLS (sees no rows)
SET ROLE rls_bypass_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- 7. ALTER ROLE to add BYPASSRLS back
ALTER ROLE rls_bypass_user BYPASSRLS;

SET ROLE rls_bypass_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- 8. BYPASSRLS with FORCE ROW LEVEL SECURITY — BYPASSRLS still bypasses
ALTER TABLE rls_bypass_test FORCE ROW LEVEL SECURITY;

SET ROLE rls_bypass_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- 9. Normal user still blocked by FORCE + RLS
SET ROLE rls_normal_user;
SELECT id, owner_name FROM rls_bypass_test ORDER BY id;
RESET ROLE;

-- Cleanup
ALTER TABLE rls_bypass_test NO FORCE ROW LEVEL SECURITY;
ALTER TABLE rls_bypass_test DISABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS see_own ON rls_bypass_test;
DROP TABLE rls_bypass_test;
DROP ROLE rls_bypass_user;
DROP ROLE rls_normal_user;

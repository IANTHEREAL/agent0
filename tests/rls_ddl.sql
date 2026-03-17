-- RLS DDL Tests
-- Purpose: Verify CREATE/ALTER/DROP POLICY, ENABLE/DISABLE/FORCE RLS,
--          and pg_catalog reflection (pg_policy, pg_class, pg_tables).

-- 1. Setup
DROP TABLE IF EXISTS rls_test;
CREATE TABLE rls_test (id INT PRIMARY KEY, user_id INT, data TEXT);

-- 2. Enable RLS
ALTER TABLE rls_test ENABLE ROW LEVEL SECURITY;

-- Verify pg_class reflects rls_enabled
SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class WHERE relname = 'rls_test';

-- Verify pg_tables reflects rowsecurity
SELECT tablename, rowsecurity
  FROM pg_tables WHERE tablename = 'rls_test';

-- 3. Create a basic PERMISSIVE SELECT policy
CREATE POLICY sel_own ON rls_test
  FOR SELECT
  USING (user_id = 1);

-- Verify pg_policy shows it
SELECT polname, polcmd, polpermissive, polroles::text,
       pg_get_expr(polqual, polrelid) AS polqual
  FROM pg_policy WHERE polname = 'sel_own';

-- 4. Create a RESTRICTIVE policy
CREATE POLICY restrict_active ON rls_test
  AS RESTRICTIVE
  FOR SELECT
  TO public
  USING (data IS NOT NULL);

SELECT polname, polcmd, polpermissive
  FROM pg_policy WHERE polname = 'restrict_active';

-- 5. Create an INSERT policy with WITH CHECK
CREATE POLICY ins_check ON rls_test
  FOR INSERT
  WITH CHECK (user_id > 0);

SELECT polname, polcmd,
       pg_get_expr(polwithcheck, polrelid) AS polwithcheck
  FROM pg_policy WHERE polname = 'ins_check';

-- 6. Create an UPDATE policy with both USING and WITH CHECK
CREATE POLICY upd_both ON rls_test
  FOR UPDATE
  USING (user_id = 1)
  WITH CHECK (user_id > 0);

SELECT polname, polcmd,
       pg_get_expr(polqual, polrelid) AS polqual,
       pg_get_expr(polwithcheck, polrelid) AS polwithcheck
  FROM pg_policy WHERE polname = 'upd_both';

-- 7. Verify duplicate policy name fails
CREATE POLICY sel_own ON rls_test FOR SELECT USING (true);

-- 8. ALTER POLICY — change USING expression
ALTER POLICY sel_own ON rls_test USING (user_id = 2);

SELECT polname, pg_get_expr(polqual, polrelid) AS polqual
  FROM pg_policy WHERE polname = 'sel_own';

-- 9. ALTER POLICY — change roles
ALTER POLICY sel_own ON rls_test TO public;

-- 10. PostgreSQL accepts ALTER POLICY with no changes as a no-op.
ALTER POLICY sel_own ON rls_test;

-- 11. DROP POLICY
DROP POLICY ins_check ON rls_test;

-- Verify it's gone
SELECT 'ins_check_count=' || COUNT(*) AS result
  FROM pg_policy WHERE polname = 'ins_check';

-- 12. DROP POLICY IF EXISTS (no error for missing)
DROP POLICY IF EXISTS ins_check ON rls_test;

-- 13. DROP POLICY non-existent without IF EXISTS (should fail)
DROP POLICY nonexistent ON rls_test;

-- 14. FORCE ROW LEVEL SECURITY
ALTER TABLE rls_test FORCE ROW LEVEL SECURITY;

SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class WHERE relname = 'rls_test';

-- 15. NO FORCE ROW LEVEL SECURITY
ALTER TABLE rls_test NO FORCE ROW LEVEL SECURITY;

SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class WHERE relname = 'rls_test';

-- 16. DISABLE ROW LEVEL SECURITY
ALTER TABLE rls_test DISABLE ROW LEVEL SECURITY;

SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class WHERE relname = 'rls_test';

-- 17. Verify SELECT/DELETE cannot have WITH CHECK
CREATE POLICY bad_sel ON rls_test FOR SELECT WITH CHECK (true);
CREATE POLICY bad_del ON rls_test FOR DELETE WITH CHECK (true);

-- 18. Verify INSERT cannot have USING
CREATE POLICY bad_ins ON rls_test FOR INSERT USING (true);

-- 19. Policy with nested subquery expression
CREATE POLICY complex_pol ON rls_test
  FOR SELECT
  USING (user_id IN (SELECT id FROM rls_test WHERE data = 'admin'));

SELECT polname, pg_get_expr(polqual, polrelid) AS polqual
  FROM pg_policy WHERE polname = 'complex_pol';

-- 20. Count all remaining policies
SELECT 'policy_count=' || COUNT(*) AS result
  FROM pg_policy
  WHERE polrelid = (SELECT oid FROM pg_class WHERE relname = 'rls_test');

-- 21. Cleanup
DROP POLICY IF EXISTS sel_own ON rls_test;
DROP POLICY IF EXISTS restrict_active ON rls_test;
DROP POLICY IF EXISTS upd_both ON rls_test;
DROP POLICY IF EXISTS complex_pol ON rls_test;
DROP TABLE rls_test;

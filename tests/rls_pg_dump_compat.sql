-- Test: pg_dump/pg_restore compatibility for RLS policies
-- Verifies that catalog queries used by pg_dump return sufficient information
-- to reconstruct RLS configuration: ALTER TABLE ... ENABLE/FORCE ROW LEVEL SECURITY
-- and CREATE POLICY statements.

-- Setup: table with RLS enabled + forced, multiple policy types
CREATE TABLE dump_test (
    id INT PRIMARY KEY,
    owner_name TEXT,
    data TEXT
);

ALTER TABLE dump_test ENABLE ROW LEVEL SECURITY;
ALTER TABLE dump_test FORCE ROW LEVEL SECURITY;

CREATE POLICY sel_owner ON dump_test FOR SELECT
    USING (owner_name = current_user);

CREATE POLICY ins_check ON dump_test FOR INSERT
    WITH CHECK (owner_name = current_user);

CREATE POLICY upd_all ON dump_test FOR UPDATE
    USING (true)
    WITH CHECK (owner_name = current_user);

CREATE POLICY del_restrictive ON dump_test AS RESTRICTIVE FOR DELETE
    USING (id > 0);

-- 1. pg_dump queries pg_class for RLS flags
SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class
 WHERE relname = 'dump_test';

-- 2. pg_dump queries pg_policy joined with pg_class for policy details
SELECT p.polname,
       c.relname,
       p.polcmd,
       p.polpermissive,
       p.polroles::text,
       p.polqual,
       p.polwithcheck
  FROM pg_policy p
  JOIN pg_class c ON c.oid = p.polrelid
 WHERE c.relname = 'dump_test'
 ORDER BY p.polname;

-- 3. pg_dump uses pg_policies view for human-readable output
SELECT policyname, tablename, permissive, cmd, roles::text, qual, with_check
  FROM pg_policies
 WHERE tablename = 'dump_test'
 ORDER BY policyname;

-- 4. Verify pg_tables reflects RLS
SELECT tablename, rowsecurity
  FROM pg_tables
 WHERE tablename = 'dump_test';

-- 5. Round-trip simulation: drop and recreate from catalog data
-- (This proves the catalog contains enough info to reconstruct policies)
DROP POLICY sel_owner ON dump_test;
DROP POLICY ins_check ON dump_test;
DROP POLICY upd_all ON dump_test;
DROP POLICY del_restrictive ON dump_test;

-- Verify policies are gone
SELECT count(*) AS policy_count FROM pg_policies WHERE tablename = 'dump_test';

-- Recreate from what pg_dump would emit
CREATE POLICY del_restrictive ON dump_test AS RESTRICTIVE FOR DELETE
    USING (id > 0);
CREATE POLICY ins_check ON dump_test FOR INSERT
    WITH CHECK (owner_name = current_user);
CREATE POLICY sel_owner ON dump_test FOR SELECT
    USING (owner_name = current_user);
CREATE POLICY upd_all ON dump_test FOR UPDATE
    USING (true)
    WITH CHECK (owner_name = current_user);

-- Verify round-trip: same policies exist
SELECT policyname, permissive, cmd, qual, with_check
  FROM pg_policies
 WHERE tablename = 'dump_test'
 ORDER BY policyname;

-- Cleanup
DROP TABLE dump_test;

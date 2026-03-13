-- RLS pg_policies view tests
-- Purpose: Verify pg_policies catalog view returns human-readable policy info.

-- 1. Setup
DROP TABLE IF EXISTS pol_test;
CREATE TABLE pol_test (id INT PRIMARY KEY, user_id TEXT NOT NULL, data TEXT);

-- 2. Create policies of different types
CREATE POLICY sel_all ON pol_test FOR SELECT USING (true);

CREATE POLICY ins_own ON pol_test FOR INSERT
  WITH CHECK (user_id = current_user);

CREATE POLICY upd_own ON pol_test FOR UPDATE
  USING (user_id = current_user)
  WITH CHECK (user_id = current_user);

CREATE POLICY del_restrict ON pol_test AS RESTRICTIVE FOR DELETE
  USING (data IS NOT NULL);

ALTER TABLE pol_test ENABLE ROW LEVEL SECURITY;

-- 3. Verify pg_policies returns all policies with correct columns
SELECT policyname, cmd, permissive, qual, with_check
  FROM pg_policies
  WHERE tablename = 'pol_test'
  ORDER BY policyname;

-- 4. Verify roles is an array (cast to text for display)
SELECT policyname, roles::text
  FROM pg_policies
  WHERE tablename = 'pol_test' AND policyname = 'sel_all';

-- 5. Verify schema name
SELECT schemaname, tablename
  FROM pg_policies
  WHERE tablename = 'pol_test'
  LIMIT 1;

-- 6. pg_policy polroles is an array (cast to text for display)
SELECT polname, polroles::text
  FROM pg_policy
  WHERE polname = 'sel_all';

-- 7. Cleanup
DROP TABLE pol_test;

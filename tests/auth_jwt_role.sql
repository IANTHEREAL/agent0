-- auth.jwt() and auth.role() Integration Tests
-- Purpose: Verify auth.jwt() returns full JWT claims and auth.role() returns
-- the current database role. Both are zero-argument functions protected by
-- the anti-spoofing guard.

-- 1. auth.jwt() returns NULL when no JWT context is set
SELECT auth.jwt() IS NULL AS jwt_is_null;

-- 2. auth.role() returns a non-null value (the current database role)
SELECT auth.role() IS NOT NULL AS role_is_not_null;

-- 3. auth.role() equals current_user
SELECT auth.role() = current_user AS role_matches_current_user;

-- 4. auth.jwt() can be used in COALESCE
SELECT COALESCE(auth.jwt(), '{}') AS effective_jwt;

-- 5. Client cannot spoof request.jwt.claims via SET (anti-spoofing)
SET "request.jwt.claims" = '{"sub":"hacker"}';

-- RLS auth.uid() Integration Tests
-- Purpose: Verify auth.uid() function reads from server-reserved auth.uid GUC.
-- Covers: NULL when unset, usage in expressions, anti-spoofing protection.

-- 1. auth.uid() returns NULL when no JWT context is set
SELECT auth.uid() IS NULL AS uid_is_null;

-- 2. auth.uid() can be used in expressions
SELECT COALESCE(auth.uid(), 'anonymous') AS effective_uid;

-- 3. Client cannot spoof auth.uid via SET (anti-spoofing)
SET "auth.uid" = 'hacker';

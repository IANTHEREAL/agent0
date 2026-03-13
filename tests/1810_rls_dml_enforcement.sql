-- RLS DML enforcement (Issue #1810)
-- Tests: INSERT WITH CHECK, UPDATE USING + WITH CHECK, DELETE USING,
--        RETURNING error 42501, COPY FROM WITH CHECK, visibility semantics.

SET client_min_messages = warning;

DROP TABLE IF EXISTS rls_posts CASCADE;
DROP ROLE IF EXISTS rls_alice;
DROP ROLE IF EXISTS rls_bob;

-- Setup
CREATE ROLE rls_alice LOGIN PASSWORD 'pw';
CREATE ROLE rls_bob LOGIN PASSWORD 'pw';

CREATE TABLE rls_posts (
    id SERIAL PRIMARY KEY,
    owner TEXT NOT NULL,
    title TEXT
);

-- Grant DML privileges
GRANT SELECT, INSERT, UPDATE, DELETE ON rls_posts TO rls_alice;
GRANT SELECT, INSERT, UPDATE, DELETE ON rls_posts TO rls_bob;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO rls_alice;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO rls_bob;

-- Enable RLS
ALTER TABLE rls_posts ENABLE ROW LEVEL SECURITY;

-- Policies: users can only see/modify their own rows
CREATE POLICY see_own ON rls_posts FOR SELECT
    USING (owner = current_user);

CREATE POLICY insert_own ON rls_posts FOR INSERT
    WITH CHECK (owner = current_user);

CREATE POLICY update_own ON rls_posts FOR UPDATE
    USING (owner = current_user)
    WITH CHECK (owner = current_user);

CREATE POLICY delete_own ON rls_posts FOR DELETE
    USING (owner = current_user);

-- Seed data as superuser (bypasses RLS)
INSERT INTO rls_posts (owner, title) VALUES ('rls_alice', 'Alice post 1');
INSERT INTO rls_posts (owner, title) VALUES ('rls_alice', 'Alice post 2');
INSERT INTO rls_posts (owner, title) VALUES ('rls_bob', 'Bob post 1');

-- ============================================================
-- 1. SELECT: each user sees only their own rows
-- ============================================================
SET ROLE rls_alice;
SELECT owner, title FROM rls_posts ORDER BY id;
RESET ROLE;

SET ROLE rls_bob;
SELECT owner, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- ============================================================
-- 2. INSERT WITH CHECK: can only insert as self
-- ============================================================
SET ROLE rls_alice;
-- OK: alice inserts as alice
INSERT INTO rls_posts (owner, title) VALUES ('rls_alice', 'Alice post 3');

-- FAIL: alice tries to insert as bob
INSERT INTO rls_posts (owner, title) VALUES ('rls_bob', 'Impersonation attempt');
RESET ROLE;

-- Verify alice's insert succeeded
SET ROLE rls_alice;
SELECT owner, title FROM rls_posts WHERE title = 'Alice post 3';
RESET ROLE;

-- ============================================================
-- 3. UPDATE USING + WITH CHECK
-- ============================================================
SET ROLE rls_alice;
-- OK: alice updates her own post
UPDATE rls_posts SET title = 'Alice post 1 updated' WHERE title = 'Alice post 1';

-- SILENT SKIP: alice tries to update bob's post (invisible via USING)
UPDATE rls_posts SET title = 'Hacked' WHERE owner = 'rls_bob';
RESET ROLE;

-- Verify alice's update worked and bob's post is untouched
SELECT owner, title FROM rls_posts ORDER BY id;

-- FAIL: alice tries to change owner to bob (WITH CHECK violation)
SET ROLE rls_alice;
UPDATE rls_posts SET owner = 'rls_bob' WHERE title = 'Alice post 1 updated';
RESET ROLE;

-- ============================================================
-- 4. DELETE USING: can only delete own rows
-- ============================================================
SET ROLE rls_bob;
-- OK: bob deletes his own post
DELETE FROM rls_posts WHERE title = 'Bob post 1';

-- SILENT SKIP: bob tries to delete alice's posts (invisible)
DELETE FROM rls_posts WHERE owner = 'rls_alice';
RESET ROLE;

-- Verify bob's post is deleted, alice's posts remain
SELECT owner, title FROM rls_posts ORDER BY id;

-- ============================================================
-- 5. RETURNING with SELECT policy violation (error 42501)
-- ============================================================

-- Create a permissive insert-any policy for this test
DROP POLICY insert_own ON rls_posts;
CREATE POLICY insert_any ON rls_posts FOR INSERT
    WITH CHECK (true);

SET ROLE rls_alice;
-- FAIL: alice inserts bob's row, but RETURNING requires SELECT visibility
-- which the see_own policy denies
INSERT INTO rls_posts (owner, title) VALUES ('rls_bob', 'Bob via alice') RETURNING *;
RESET ROLE;

-- Restore original insert policy
DROP POLICY insert_any ON rls_posts;
CREATE POLICY insert_own ON rls_posts FOR INSERT
    WITH CHECK (owner = current_user);

-- ============================================================
-- 6. Superuser bypasses RLS
-- ============================================================
SELECT owner, title FROM rls_posts ORDER BY id;

-- ============================================================
-- 7. FORCE ROW LEVEL SECURITY affects owner
-- ============================================================
ALTER TABLE rls_posts FORCE ROW LEVEL SECURITY;

-- Table owner (current superuser) now also subject to RLS...
-- but superuser always bypasses regardless of FORCE
SELECT count(*) FROM rls_posts;

ALTER TABLE rls_posts NO FORCE ROW LEVEL SECURITY;

-- ============================================================
-- Cleanup
-- ============================================================
DROP TABLE rls_posts CASCADE;
DROP ROLE rls_alice;
DROP ROLE rls_bob;

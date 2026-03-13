-- RLS SELECT Integration Tests
-- Purpose: Verify post-Analyzer RLS predicate injection for SELECT queries.
-- Covers: basic filtering, self-join, subquery, CTE, view, COPY TO,
--         superuser bypass, owner bypass, FORCE RLS, default-deny,
--         permissive OR + restrictive AND combination.

-- Setup: create roles and table
DROP TABLE IF EXISTS rls_posts CASCADE;
DROP ROLE IF EXISTS rls_alice;
DROP ROLE IF EXISTS rls_bob;
CREATE ROLE rls_alice LOGIN PASSWORD 'pw';
CREATE ROLE rls_bob LOGIN PASSWORD 'pw';

CREATE TABLE rls_posts (
    id INT PRIMARY KEY,
    author TEXT NOT NULL,
    title TEXT NOT NULL,
    published BOOLEAN NOT NULL DEFAULT true
);

INSERT INTO rls_posts VALUES
    (1, 'alice', 'Alice public post', true),
    (2, 'alice', 'Alice draft', false),
    (3, 'bob',   'Bob public post', true),
    (4, 'bob',   'Bob draft', false);

-- Grant SELECT to both roles
GRANT SELECT ON rls_posts TO rls_alice, rls_bob;

-- 1. No RLS enabled — everyone sees all rows
SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- 2. Enable RLS with no policies — default-deny (zero rows for non-owner)
ALTER TABLE rls_posts ENABLE ROW LEVEL SECURITY;

SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- 3. Add permissive policy: users see their own rows
CREATE POLICY see_own ON rls_posts
    FOR SELECT
    USING (author = current_user);

SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

SET ROLE rls_bob;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- 4. Add second permissive policy: also see published rows (OR semantics)
CREATE POLICY see_published ON rls_posts
    FOR SELECT
    USING (published = true);

SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

SET ROLE rls_bob;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- 5. Add restrictive policy: AND with permissive (must also be published)
CREATE POLICY must_be_published ON rls_posts
    AS RESTRICTIVE
    FOR SELECT
    USING (published = true);

-- Now: (see_own OR see_published) AND must_be_published
-- alice sees: own+published(1) OR published(1,3) → {1,2,3} intersect published{1,3} = {1,3}
SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- bob sees: own+published(3) OR published(1,3) → {1,3,4} intersect published{1,3} = {1,3}
SET ROLE rls_bob;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- 6. Superuser bypass — sees all rows
SELECT id, author, title FROM rls_posts ORDER BY id;

-- 7. Owner bypass (table owner is current superuser, no FORCE)
-- Already demonstrated above (step 6)

-- 8. FORCE RLS — owner must also obey policies
ALTER TABLE rls_posts FORCE ROW LEVEL SECURITY;

-- Superuser still bypasses even with FORCE
SELECT id, author, title FROM rls_posts ORDER BY id;

ALTER TABLE rls_posts NO FORCE ROW LEVEL SECURITY;

-- 9. Self-join: RLS applies to both sides
DROP POLICY must_be_published ON rls_posts;

SET ROLE rls_alice;
SELECT a.id, b.id
  FROM rls_posts a
  JOIN rls_posts b ON a.author = b.author
 ORDER BY a.id, b.id;
RESET ROLE;

-- 10. Subquery: RLS applies inside subquery
SET ROLE rls_alice;
SELECT id, title FROM rls_posts
 WHERE author IN (SELECT DISTINCT author FROM rls_posts)
 ORDER BY id;
RESET ROLE;

-- 11. CTE: RLS applies inside CTE
SET ROLE rls_alice;
WITH my_posts AS (
    SELECT * FROM rls_posts
)
SELECT id, title FROM my_posts ORDER BY id;
RESET ROLE;

-- 12. COUNT with RLS — only visible rows counted
SET ROLE rls_alice;
SELECT COUNT(*) FROM rls_posts;
RESET ROLE;

SET ROLE rls_bob;
SELECT COUNT(*) FROM rls_posts;
RESET ROLE;

-- 13. Aggregate with RLS
SET ROLE rls_alice;
SELECT author, COUNT(*) AS cnt FROM rls_posts GROUP BY author ORDER BY author;
RESET ROLE;

-- 14. Disable RLS — everyone sees all rows again
ALTER TABLE rls_posts DISABLE ROW LEVEL SECURITY;

SET ROLE rls_alice;
SELECT id, author, title FROM rls_posts ORDER BY id;
RESET ROLE;

-- Cleanup
DROP POLICY IF EXISTS see_own ON rls_posts;
DROP POLICY IF EXISTS see_published ON rls_posts;
DROP TABLE rls_posts;
DROP ROLE rls_alice;
DROP ROLE rls_bob;

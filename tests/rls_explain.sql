-- RLS EXPLAIN Integration Tests (P2-6)
-- Purpose: Verify that EXPLAIN output shows RLS-injected predicates.
-- RLS predicates are injected post-Analyzer, pre-optimizer, so they
-- naturally appear in EXPLAIN plans as Filter conditions.

-- Setup
DROP TABLE IF EXISTS rls_exp CASCADE;
DROP ROLE IF EXISTS rls_exp_user;
CREATE ROLE rls_exp_user LOGIN PASSWORD 'pw';

CREATE TABLE rls_exp (
    id INT PRIMARY KEY,
    owner TEXT NOT NULL,
    data TEXT
);

INSERT INTO rls_exp VALUES
    (1, 'rls_exp_user', 'visible'),
    (2, 'other',        'hidden'),
    (3, 'rls_exp_user', 'also visible');

GRANT SELECT ON rls_exp TO rls_exp_user;

-- 1. EXPLAIN without RLS — no RLS filter in plan
SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- 2. Enable RLS with no policies — default-deny injects WHERE false
ALTER TABLE rls_exp ENABLE ROW LEVEL SECURITY;

SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- 3. Add permissive policy — EXPLAIN shows owner = current_user filter
CREATE POLICY own_rows ON rls_exp
    FOR SELECT
    USING (owner = current_user);

SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- 4. Verify actual results match the plan's filter
SET ROLE rls_exp_user;
SELECT id, owner, data FROM rls_exp ORDER BY id;
RESET ROLE;

-- 5. Add restrictive policy — EXPLAIN shows both predicates (AND)
CREATE POLICY restrict_id ON rls_exp AS RESTRICTIVE
    FOR SELECT
    USING (id > 1);

SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- 6. Verify restrictive filter narrows results
SET ROLE rls_exp_user;
SELECT id, owner, data FROM rls_exp ORDER BY id;
RESET ROLE;

-- 7. EXPLAIN with WHERE clause — shows both user WHERE and RLS filter
SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp WHERE data = 'visible';
RESET ROLE;

-- 8. Superuser sees no RLS filter in EXPLAIN
EXPLAIN SELECT * FROM rls_exp;

-- 9. EXPLAIN ANALYZE also shows RLS filters
SET ROLE rls_exp_user;
EXPLAIN (ANALYZE) SELECT * FROM rls_exp WHERE id = 1;
RESET ROLE;

-- Cleanup
DROP TABLE rls_exp CASCADE;
DROP ROLE rls_exp_user;

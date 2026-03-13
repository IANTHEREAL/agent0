-- RLS EXPLAIN Integration Tests (P2-6)
-- Purpose: Verify that EXPLAIN output shows RLS-injected predicates.
-- RLS predicates are injected post-Analyzer, pre-optimizer, so they
-- naturally appear in EXPLAIN plans as Filter conditions.
--
-- Each query produces output unique to itself so that global .assert
-- substring matching can validate per-query behavior.

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

-- Enable RLS
ALTER TABLE rls_exp ENABLE ROW LEVEL SECURITY;

-- Q1. Default-deny: no policies — EXPLAIN shows WHERE false
SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- Q2. Add permissive policy — EXPLAIN shows owner = current_user filter
CREATE POLICY own_rows ON rls_exp
    FOR SELECT
    USING (owner = current_user);

SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- Q3. Verify actual results: permissive only → ids 1,3
SET ROLE rls_exp_user;
SELECT 'q3:' || string_agg(id::text, ',' ORDER BY id) AS result FROM rls_exp;
RESET ROLE;

-- Q4. Add restrictive policy — EXPLAIN shows both predicates (AND)
CREATE POLICY restrict_id ON rls_exp AS RESTRICTIVE
    FOR SELECT
    USING (id > 1);

SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp;
RESET ROLE;

-- Q5. Verify actual results: restrictive narrows → only id 3
SET ROLE rls_exp_user;
SELECT 'q5:' || string_agg(id::text, ',' ORDER BY id) AS result FROM rls_exp;
RESET ROLE;

-- Q6. EXPLAIN with WHERE clause — shows user WHERE AND RLS filter
SET ROLE rls_exp_user;
EXPLAIN SELECT * FROM rls_exp WHERE data = 'visible';
RESET ROLE;

-- Cleanup
DROP TABLE rls_exp CASCADE;
DROP ROLE rls_exp_user;

-- RLS Security Barrier Regression Test (P3-2 follow-up)
-- Purpose: Verify that the security barrier subquery survives the full
-- analyze → rewrite → RLS-inject pipeline, preventing user predicates
-- from observing RLS-hidden rows.
--
-- The key invariant: a user-supplied WHERE predicate must never evaluate
-- on rows that are hidden by RLS. Without the barrier, the optimizer
-- could push user predicates below RLS filters, allowing a side-channel
-- function to observe hidden data.
--
-- Test strategy: use a logging function that records which rows it sees
-- into a side table. If the barrier is working, the function only sees
-- RLS-visible rows. If the barrier is broken (flattened), the function
-- may see all rows.

-- Setup
DROP TABLE IF EXISTS barrier_data CASCADE;
DROP TABLE IF EXISTS barrier_log CASCADE;
DROP FUNCTION IF EXISTS barrier_spy(text);
DROP ROLE IF EXISTS barrier_user;
CREATE ROLE barrier_user LOGIN PASSWORD 'pw';

CREATE TABLE barrier_data (
    id INT PRIMARY KEY,
    owner_name TEXT NOT NULL,
    secret TEXT NOT NULL
);

-- Side table for logging which rows the spy function sees
CREATE TABLE barrier_log (
    seen_secret TEXT NOT NULL
);

INSERT INTO barrier_data VALUES
    (1, 'barrier_user', 'visible-1'),
    (2, 'barrier_user', 'visible-2'),
    (3, 'other_user',   'hidden-1'),
    (4, 'other_user',   'hidden-2');

GRANT SELECT ON barrier_data TO barrier_user;
GRANT SELECT, INSERT ON barrier_log TO barrier_user;

-- Enable RLS: users only see their own rows
ALTER TABLE barrier_data ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_rows ON barrier_data
    FOR SELECT
    USING (owner_name = current_user);

-- Create a "spy" function that logs the secret it receives.
-- If the security barrier is working, this function should only be
-- called with secrets from RLS-visible rows ('visible-1', 'visible-2').
-- If the barrier is broken, it might also see 'hidden-1', 'hidden-2'.
CREATE FUNCTION barrier_spy(val text) RETURNS boolean
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO barrier_log (seen_secret) VALUES (val);
    RETURN true;
END;
$$;

-- Q1: Query with user predicate that calls the spy function.
-- The spy logs every secret value it evaluates.
SET ROLE barrier_user;
SELECT id, secret FROM barrier_data WHERE barrier_spy(secret) ORDER BY id;
RESET ROLE;

-- Q2: Check what the spy function saw.
-- With a working security barrier, it should only see visible-1 and visible-2.
-- If the barrier was flattened, it might also see hidden-1 and hidden-2.
SELECT seen_secret FROM barrier_log ORDER BY seen_secret;

-- Q3: Verify actual query results (only visible rows returned)
SET ROLE barrier_user;
SELECT id, secret FROM barrier_data ORDER BY id;
RESET ROLE;

-- Q4: Test with user WHERE + RLS together (compound predicate)
TRUNCATE barrier_log;
SET ROLE barrier_user;
SELECT id, secret FROM barrier_data WHERE barrier_spy(secret) AND id > 1 ORDER BY id;
RESET ROLE;

-- Q5: Check spy log — should only see visible-1 and visible-2 (or just visible-2
-- depending on evaluation order), but never hidden rows.
SELECT seen_secret FROM barrier_log ORDER BY seen_secret;

-- Cleanup
DROP FUNCTION IF EXISTS barrier_spy(text);
DROP TABLE IF EXISTS barrier_log CASCADE;
DROP TABLE IF EXISTS barrier_data CASCADE;
DROP ROLE IF EXISTS barrier_user;

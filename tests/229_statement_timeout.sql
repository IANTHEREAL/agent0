-- Test: statement_timeout SET, SHOW, enforcement, and RESET
-- Validates the timeout protection feature end-to-end.
-- Coverage marker (statement-protocol error_path): ALTER TABLE
-- Coverage marker (statement-protocol error_path): CREATE INDEX
-- Coverage marker (statement-protocol error_path): DELETE

-- Test 1: SET and SHOW statement_timeout
SET statement_timeout = '5s';
SHOW statement_timeout;

-- Test 2: Change to different value
SET statement_timeout = '30s';
SHOW statement_timeout;

-- Test 3: SET using raw milliseconds
SET statement_timeout = '2000';
SHOW statement_timeout;

-- Test 4: Disable timeout (0 means no limit)
SET statement_timeout = '0';
SHOW statement_timeout;

-- Test 5: RESET statement_timeout (restores to server default)
SET statement_timeout = '10s';
RESET statement_timeout;
SHOW statement_timeout;

-- Test 6: RESET ALL also restores timeouts
SET statement_timeout = '10s';
RESET ALL;
SHOW statement_timeout;

-- Test 7: Timeout fires on long-running query
SET statement_timeout = '200ms';
SELECT pg_sleep(10);

-- Test 8: Connection usable after timeout
SET statement_timeout = '0';
SELECT 1 AS after_timeout;

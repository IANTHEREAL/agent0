-- Test: idle_in_transaction_session_timeout SET, SHOW, and RESET
-- Note: Actual timeout enforcement cannot be tested via SQL files because
-- the check happens between commands, and psql pipelines commands without delay.
-- Enforcement is verified by unit tests and manual QA.

-- Test 1: Default SHOW (0 means disabled when no server override)
SHOW idle_in_transaction_session_timeout;

-- Test 2: SET and SHOW
SET idle_in_transaction_session_timeout = '2s';
SHOW idle_in_transaction_session_timeout;

-- Test 3: Change value
SET idle_in_transaction_session_timeout = '30s';
SHOW idle_in_transaction_session_timeout;

-- Test 4: SET using raw milliseconds
SET idle_in_transaction_session_timeout = '5000';
SHOW idle_in_transaction_session_timeout;

-- Test 5: Disable
SET idle_in_transaction_session_timeout = '0';
SHOW idle_in_transaction_session_timeout;

-- Test 6: RESET
SET idle_in_transaction_session_timeout = '10s';
RESET idle_in_transaction_session_timeout;
SHOW idle_in_transaction_session_timeout;

-- Test 7: RESET ALL
SET idle_in_transaction_session_timeout = '10s';
RESET ALL;
SHOW idle_in_transaction_session_timeout;

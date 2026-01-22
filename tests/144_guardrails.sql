-- PostgreSQL compatible tests from guardrails
-- 2 tests

-- Test 1: statement (line 5)
SET client_min_messages = warning;
DROP TABLE IF EXISTS guardrails;
CREATE TABLE guardrails (i INT PRIMARY KEY);
INSERT INTO guardrails SELECT generate_series(1, 100);

-- Test 2: statement (line 11)
-- CockroachDB-specific guardrail; PostgreSQL has no equivalent.
-- SET transaction_rows_read_err = 1;

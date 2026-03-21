-- GUC baseline: hollow GUC acceptance tests.
-- P0 tests for #1962 GUC architecture refactoring.
-- These verify that pg_dump-emitted SET statements are accepted.

\pset tuples_only on

-- ============================================================
-- pg_dump preamble GUCs (must all succeed without error)
-- ============================================================

SET statement_timeout = 0;
SELECT 'statement_timeout_ok' AS test;

SET lock_timeout = 0;
SELECT 'lock_timeout_ok' AS test;

SET idle_in_transaction_session_timeout = 0;
SELECT 'idle_in_txn_ok' AS test;

SET client_encoding = 'UTF8';
SELECT 'client_encoding_ok' AS test;

SET standard_conforming_strings = on;
SELECT 'scs_on_ok' AS test;

SET check_function_bodies = false;
SELECT 'check_fn_bodies_ok' AS test;

SET client_min_messages = warning;
SELECT 'client_min_messages_ok' AS test;

SET row_security = off;
SELECT 'row_security_ok' AS test;

SET default_tablespace = '';
SELECT 'default_tablespace_empty_ok' AS test;

SET default_table_access_method = heap;
SELECT 'default_tam_heap_ok' AS test;

-- ============================================================
-- DateStyle acceptance (pg_dump sends this)
-- ============================================================
SET DateStyle = 'ISO, MDY';
SELECT 'datestyle_ok' AS test;

-- ============================================================
-- SHOW hollow GUCs returns stored values
-- ============================================================
SHOW standard_conforming_strings;
SHOW check_function_bodies;
SHOW default_tablespace;
SHOW default_table_access_method;

-- ============================================================
-- Verify current behavior for values that should be rejected post-refactoring
-- (documenting current behavior as baseline)
-- ============================================================

-- Cleanup
RESET statement_timeout;
RESET lock_timeout;
RESET idle_in_transaction_session_timeout;
RESET client_encoding;
RESET standard_conforming_strings;
RESET check_function_bodies;
RESET client_min_messages;
RESET row_security;
RESET default_tablespace;
RESET default_table_access_method;

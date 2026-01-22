-- PostgreSQL compatible tests from int_size
-- 41 tests
--
-- CockroachDB has settings like `default_int_size` and `serial_normalization`.
-- PostgreSQL does not; integer sizes are fixed and `SERIAL` always uses sequences.
-- This file asserts the PostgreSQL behavior.

\set ON_ERROR_STOP 1
SET client_min_messages = warning;

-- Test 1: query (line 4)
SELECT pg_typeof(1::INT) AS int_type, pg_column_size(1::INT) AS int_bytes;

-- Test 2: query (line 11)
SELECT pg_typeof(1::BIGINT) AS bigint_type, pg_column_size(1::BIGINT) AS bigint_bytes;

-- Test 3: statement (line 21)
CREATE TABLE i4 (i4 INT);

-- Test 4: query (line 25)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'i4'
ORDER BY ordinal_position;

-- Test 5: statement (line 54)
CREATE TABLE i8 (i8 BIGINT);

-- Test 6: query (line 58)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'i8'
ORDER BY ordinal_position;

-- Test 7: statement (line 131)
CREATE TABLE i4_serial (a SERIAL);

-- Test 8: query (line 135)
SELECT column_name, data_type, column_default LIKE 'nextval(%' AS has_nextval
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'i4_serial'
ORDER BY ordinal_position;

-- Test 9: statement (line 157)
CREATE TABLE i8_serial (a BIGSERIAL);

-- Test 10: query (line 161)
SELECT column_name, data_type, column_default LIKE 'nextval(%' AS has_nextval
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'i8_serial'
ORDER BY ordinal_position;

DROP TABLE i8_serial;
DROP TABLE i4_serial;
DROP TABLE i8;
DROP TABLE i4;

RESET client_min_messages;

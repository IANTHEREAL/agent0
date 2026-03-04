-- System Functions Tests

\connect postgres

SELECT CURRENT_USER;
SELECT SESSION_USER;
SELECT * FROM CURRENT_USER AS t(u);
SELECT * FROM USER AS t(u);
SELECT CURRENT_DATABASE();
SELECT * FROM CURRENT_DATABASE() AS t(db);
SELECT CURRENT_SCHEMA();
SELECT * FROM CURRENT_SCHEMA() AS t(schema_name);

SELECT version() IS NOT NULL AS has_version;

SELECT PG_TYPEOF(1) AS int_type;
SELECT PG_TYPEOF(1.5) AS numeric_type;
SELECT PG_TYPEOF('hello'::TEXT) AS text_type;
SELECT PG_TYPEOF(TRUE) AS bool_type;
SELECT PG_TYPEOF(ARRAY[1,2,3]) AS array_type;
SELECT PG_TYPEOF('{"a":1}'::JSONB) AS jsonb_type;

SELECT PG_COLUMN_SIZE(1::INT) AS int_size;
SELECT PG_COLUMN_SIZE('hello'::TEXT) AS text_size;

SELECT QUOTE_IDENT('column') AS normal_ident;
SELECT QUOTE_IDENT('Column Name') AS spaced_ident;
SELECT QUOTE_IDENT('select') AS reserved_ident;

SELECT QUOTE_LITERAL('hello') AS normal_literal;
SELECT QUOTE_LITERAL('it''s') AS escaped_literal;
SELECT QUOTE_LITERAL(NULL) AS null_literal;

SELECT QUOTE_NULLABLE('hello') AS normal_nullable;
SELECT QUOTE_NULLABLE(NULL) AS null_nullable;

SELECT OBJ_DESCRIPTION('pg_class'::REGCLASS, 'pg_class') IS NULL AS no_desc;

SELECT PG_TABLE_IS_VISIBLE('pg_class'::REGCLASS) AS pg_class_visible;

SELECT FORMAT('%s, %s!', 'Hello', 'World') AS formatted;
SELECT FORMAT('%1$s %2$s %1$s', 'A', 'B') AS positional;
SELECT FORMAT('%10s', 'test') AS padded;
SELECT FORMAT('%.3s', 'hello') AS truncated;
SELECT FORMAT('%I', 'column_name') AS identifier;
SELECT FORMAT('%L', 'value''s') AS literal;

SELECT TXID_CURRENT() > 0 AS has_txid;

SELECT CLOCK_TIMESTAMP() IS NOT NULL AS has_clock_timestamp;
SELECT STATEMENT_TIMESTAMP() IS NOT NULL AS has_statement_timestamp;
SELECT TRANSACTION_TIMESTAMP() IS NOT NULL AS has_transaction_timestamp;

DROP TABLE IF EXISTS sys_test CASCADE;
CREATE TABLE sys_test (id INT PRIMARY KEY, val TEXT);
INSERT INTO sys_test VALUES (1, 'test');

SELECT HAS_TABLE_PRIVILEGE(CURRENT_USER, 'sys_test', 'SELECT') AS can_select;

DROP TABLE sys_test;

-- pg_backend_pid() tests
SELECT pg_backend_pid() IS NOT NULL AS has_pid;
SELECT pg_typeof(pg_backend_pid()) AS pid_type;
SELECT pg_backend_pid() = pg_backend_pid() AS pid_stable;

-- pg_postmaster_start_time() tests
SELECT pg_postmaster_start_time() IS NOT NULL AS has_start_time;
SELECT pg_typeof(pg_postmaster_start_time()) AS start_time_type;

SELECT 'System functions tests completed' AS result;

# TIPG SQL Specification

This document summarizes the SQL syntax, features, and behavioral notes for TiPG (pg-tikv),
based on the current codebase and bundled tests.

Sources consulted:
- docs/sql-reference.md
- docs/architecture.md
- docs/multi-tenancy.md
- docs/authentication.md
- docs/design/*
- src/sql/*
- src/types/*
- src/protocol/handler.rs
- src/storage/encoding.rs

## 1. Overview

TiPG is a PostgreSQL-wire-compatible server that executes SQL on top of TiKV.
The request flow is:

psql/ORM -> pgwire -> sqlparser -> executor -> TiKV store

All persistent data is isolated per TiKV keyspace (tenant).

## 2. Connection and Multi-Tenancy

- Username routing: `tenant.user` or `tenant:user` selects TiKV keyspace.
- Without a tenant prefix, connections go to the default keyspace.
- Each keyspace has its own users/roles and metadata.

## 3. Data Types

Supported column types (internal names in parentheses):
- BOOLEAN (Boolean)
- INT/INTEGER/INT4 (Int32)
- BIGINT/INT8 (Int64)
- REAL/FLOAT4 (Float64)
- DOUBLE PRECISION/FLOAT8 (Float64)
- TEXT/VARCHAR/CHAR (Text)
- BYTEA (Bytes)
- TIMESTAMP / TIMESTAMPTZ (Timestamp, stored as UTC millis)
- INTERVAL (Interval, stored as millis)
- DATE (Date, days since 1970-01-01)
- TIME (Time, microseconds since midnight)
- UUID (Uuid)
- JSON / JSONB (Json/Jsonb)
- NUMERIC/DECIMAL (Numeric with optional precision/scale, backed by rust_decimal)
- ARRAY (Array of supported element types)
- VECTOR(n) (Vector with fixed dimension)
- USER-DEFINED ENUM (UserDefined)

Notes:
- Composite types can be created but cannot be used as a column type.
- NUMERIC/DECIMAL precision is limited by rust_decimal (max 28 digits).

## 4. DDL (Schema)

### Tables
- CREATE TABLE / CREATE TABLE IF NOT EXISTS
  - Column options: PRIMARY KEY, NOT NULL, UNIQUE, DEFAULT
  - Table constraints: PRIMARY KEY (...), UNIQUE, CHECK, FOREIGN KEY
  - SERIAL / BIGSERIAL columns create implicit sequences
- ALTER TABLE
  - Add/Drop/Rename columns
  - Rename table
  - Add/Drop constraints (PK/UNIQUE/CHECK/FK)
- DROP TABLE / DROP TABLE IF EXISTS
- TRUNCATE TABLE

### Indexes
- CREATE INDEX / CREATE UNIQUE INDEX
- CREATE INDEX IF NOT EXISTS
- DROP INDEX / DROP INDEX IF EXISTS
- Note: GIST indexes are not supported (explicitly rejected).

### Views
- CREATE VIEW / CREATE OR REPLACE VIEW
- DROP VIEW / DROP VIEW IF EXISTS
- CREATE MATERIALIZED VIEW
- REFRESH MATERIALIZED VIEW
- DROP MATERIALIZED VIEW

### Schemas
- CREATE SCHEMA / DROP SCHEMA (incl. IF EXISTS)
- search_path supported via SET search_path

### User-Defined Types
- CREATE TYPE ... AS ENUM
- CREATE TYPE ... AS (composite)
- DROP TYPE / DROP TYPE IF EXISTS

### Sequences
- CREATE SEQUENCE / DROP SEQUENCE
- nextval(text|regclass), currval(text|regclass), setval(text|regclass, value[, is_called])
- SERIAL/BIGSERIAL use implicit sequences

### Functions / Procedures / Triggers
- CREATE FUNCTION / CREATE OR REPLACE FUNCTION / DROP FUNCTION
  - Stores definition; validates PL/pgSQL bodies
- CREATE PROCEDURE / DROP PROCEDURE / CALL
  - PL/pgSQL subset supported (DECLARE/BEGIN/END/IF/RETURN/RAISE/assignment)
- CREATE TRIGGER / DROP TRIGGER
  - Triggers are stored but not executed (DDL compatibility)

## 5. DML (Data Manipulation)

- INSERT (single/multi-row)
- INSERT ... RETURNING (explicit columns or *)
- INSERT ... ON CONFLICT DO UPDATE
- UPDATE ... WHERE ... RETURNING
- DELETE ... WHERE ... RETURNING
- COPY FROM STDIN via pgwire COPY protocol (COPY SQL text is not parsed by executor)

## 6. Query Features

### SELECT and Filtering
- SELECT * / SELECT columns
- SELECT DISTINCT / DISTINCT ON
- WHERE with:
  - comparison (=, <, >, <=, >=, <>)
  - AND / OR / NOT
  - BETWEEN / IN (list or subquery) / EXISTS
  - LIKE / ILIKE
  - IS NULL / IS NOT NULL
- SELECT ... FOR UPDATE (row locking)

### ORDER / LIMIT / OFFSET / FETCH
- ORDER BY (ASC/DESC)
- LIMIT / OFFSET
- FETCH FIRST ... ROWS ONLY

### GROUP BY / HAVING
- GROUP BY expressions
- HAVING filters
- Aggregates: COUNT, SUM, AVG, MIN, MAX, STRING_AGG, ARRAY_AGG

### JOINs
- INNER JOIN
- LEFT / RIGHT / FULL OUTER JOIN
- NATURAL JOIN
- CROSS JOIN

### Subqueries and CTEs
- Scalar subqueries, IN, EXISTS
- WITH (CTE)
- WITH RECURSIVE (UNION / UNION ALL recursion)

### Set Operations
- UNION / UNION ALL
- INTERSECT / INTERSECT ALL
- EXCEPT / EXCEPT ALL

### Window Functions
- ROW_NUMBER, RANK, DENSE_RANK
- SUM/AVG/COUNT/MIN/MAX OVER
- LAG / LEAD
- Notes: window functions materialize full result sets (non-streaming).

### EXPLAIN
- EXPLAIN
- EXPLAIN ANALYZE (basic support via SQL executor)

## 7. Transactions

- BEGIN / COMMIT / ROLLBACK
- SAVEPOINT / ROLLBACK TO SAVEPOINT / RELEASE SAVEPOINT
- SET TRANSACTION (accepted; currently a no-op)

## 7. Expressions and Operators

- Arithmetic: +, -, *, /, %
- Unary: +, -, NOT
- CASE WHEN / THEN / ELSE
- CAST and typed literals
- JSON operators: ->, ->>, @>, <@
- Array literals and indexing
- ANY / ALL (array operand)

## 8. Built-in Functions (Representative)

Documented and implemented (non-exhaustive, see docs/sql-reference.md):
- String: upper, lower, length, concat, left, right, substring, trim, ltrim, rtrim,
  lpad, rpad, replace, reverse, repeat, split_part, initcap, position
- Math: abs, ceil, floor, round, sqrt, power, exp, ln, log, sign, mod, pi, random,
  greatest, least
- Date/time: now, current_timestamp, current_date, date_trunc, extract, age, to_char
- Conditional: coalesce, nullif
- UUID: gen_random_uuid, uuid_generate_v4

Notes:
- Some PostgreSQL built-ins may be stubbed or partially implemented.
- Unsupported function calls return errors at evaluation time.

## 9. Constraints and Referential Integrity

- PRIMARY KEY and UNIQUE constraints enforced.
- NOT NULL enforced on insert/update.
- DEFAULT expressions are evaluated on missing columns.
- CHECK constraints are parsed and enforced at DML time.
- FOREIGN KEY constraints:
  - Enforced on insert/update.
  - ON DELETE actions: CASCADE, SET NULL, SET DEFAULT, RESTRICT/NO ACTION.

## 10. System Catalogs and Information Schema

Virtual tables supported (subset):
- information_schema: tables, columns, schemata, table_constraints, key_column_usage,
  referential_constraints, constraint_column_usage, check_constraints
- pg_catalog: pg_type, pg_enum, pg_class, pg_index, pg_attribute, pg_namespace,
  pg_proc, pg_trigger, pg_description, pg_constraint, pg_am, pg_indexes, pg_range

## 11. Authentication and RBAC

- CREATE ROLE / ALTER ROLE / DROP ROLE
- GRANT / REVOKE on tables, schemas, and global scope
- Default superuser per keyspace: admin / admin

## 12. Known Limitations (Current)

Executor-level skips or explicit rejections:
- CREATE/ALTER/DROP DATABASE
- CREATE DOMAIN / CREATE AGGREGATE
- ALTER TYPE / ALTER DOMAIN / ALTER AGGREGATE / ALTER FUNCTION / ALTER SEQUENCE
- ALTER TABLE ... OWNER TO
- Dollar-quoted strings ($$...$$) not supported
- GIST index not supported
- COPY SQL is not parsed by executor (use pgwire COPY protocol instead)
- COPY TO is not supported

Behavioral notes:
- Window functions are not streaming.
- Composite types are stored but not usable as column types.
- Trigger execution is not implemented (DDL only).

## 13. Environment Variables

- PD_ENDPOINTS: TiKV PD endpoints (default 127.0.0.1:2379)
- PG_PORT: listen port (default 5433)
- PG_KEYSPACE: default TiKV keyspace

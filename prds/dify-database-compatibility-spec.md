# Dify PostgreSQL Compatibility Specification

This document describes the PostgreSQL features and SQL patterns required to support Dify as a workload. A pg-compatible database must implement these features to run Dify successfully.

## Table of Contents

1. [Overview](#overview)
2. [Required Extensions](#required-extensions)
3. [Custom Functions](#custom-functions)
4. [Data Types](#data-types)
5. [Index Types](#index-types)
6. [SQL Features](#sql-features)
7. [DDL Patterns](#ddl-patterns)
8. [DML Patterns](#dml-patterns)
9. [Connection & Transaction Patterns](#connection--transaction-patterns)
10. [Compatibility Checklist](#compatibility-checklist)

---

## Overview

- **Source**: Dify v1.x codebase analysis
- **PostgreSQL Version**: 16.x (tested with 16.11)
- **Tables**: 80+ tables
- **Schema Size**: ~4,750 lines of DDL
- **ORM**: SQLAlchemy 2.x with Flask-SQLAlchemy

---

## Required Extensions

### uuid-ossp

```sql
CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA public;
```

**Functions used**:
- `uuid_generate_v4()` - Primary key generation for most tables

---

## Custom Functions

### uuidv7()

Generates UUID v7 values with embedded timestamps for time-ordered primary keys.

```sql
CREATE FUNCTION public.uuidv7() RETURNS uuid
    LANGUAGE sql PARALLEL SAFE
    AS $$
SELECT encode(
    set_bit(
        set_bit(
            overlay(uuid_send(gen_random_uuid()) placing
                substring(int8send((extract(epoch from clock_timestamp()) * 1000)::bigint) from 3)
            from 1 for 6),
        52, 1),
    53, 1), 'hex')::uuid;
$$;
```

**Required PostgreSQL functions**:
- `gen_random_uuid()` - Generate random UUID (built-in since PG13)
- `uuid_send()` - Convert UUID to bytea
- `int8send()` - Convert bigint to bytea
- `extract(epoch from ...)` - Extract Unix timestamp
- `clock_timestamp()` - Current timestamp (changes within statement)
- `set_bit()` - Set individual bits in bytea
- `overlay()` - Replace substring in bytea
- `encode(..., 'hex')` - Convert bytea to hex string

### uuidv7_boundary()

Generates a boundary UUIDv7 for a given timestamp (useful for partitioning).

```sql
CREATE FUNCTION public.uuidv7_boundary(timestamp with time zone) RETURNS uuid
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    AS $_$
SELECT encode(
    overlay('\x00000000000070008000000000000000'::bytea
        placing substring(int8send(floor(extract(epoch from $1) * 1000)::bigint) from 3)
    from 1 for 6),
'hex')::uuid;
$_$;
```

---

## Data Types

### Scalar Types

| Type | Usage | Example Tables |
|------|-------|----------------|
| `uuid` | Primary keys, foreign keys | All tables |
| `character varying(n)` | Short strings (names, types, status) | `accounts`, `apps`, `tenants` |
| `text` | Long strings (content, descriptions) | `messages`, `documents`, `workflows` |
| `integer` | Counters, positions | `document_segments.position` |
| `bigint` | Large counters, tokens | `workflow_runs.total_tokens` |
| `double precision` | Floating point (scores, latency) | `messages.provider_response_latency` |
| `numeric(p,s)` | Precise decimals (prices) | `messages.total_price` (10,7) |
| `boolean` | Flags | `apps.is_public`, `accounts.is_active` |
| `timestamp without time zone` | Timestamps (UTC) | `created_at`, `updated_at` |
| `bytea` | Binary data (embeddings, task results) | `embeddings.embedding`, `celery_taskmeta.result` |

### JSON Types

| Type | Usage | Example Columns |
|------|-------|-----------------|
| `json` | Structured data (rarely queried) | `conversations.inputs`, `operation_logs.content` |
| `jsonb` | Queryable JSON with indexes | `documents.doc_metadata`, `datasets.retrieval_model` |

**JSONB is critical** - several tables use JSONB with GIN indexes for metadata queries.

---

## Index Types

### B-tree Indexes (Default)

Standard indexes for equality and range queries.

```sql
-- Simple index
CREATE INDEX account_email_idx ON public.accounts USING btree (email);

-- Composite index
CREATE INDEX api_token_app_id_type_idx ON public.api_tokens USING btree (app_id, type);

-- Composite with mixed ordering
CREATE INDEX workflow_node_executions_tenant_id_idx 
    ON public.workflow_node_executions 
    USING btree (tenant_id, workflow_id, node_id, created_at DESC);
```

### GIN Indexes (JSONB)

Required for JSONB column queries.

```sql
-- GIN index on JSONB column
CREATE INDEX document_metadata_idx ON public.documents USING gin (doc_metadata);
CREATE INDEX retrieval_model_idx ON public.datasets USING gin (retrieval_model);
CREATE INDEX source_info_idx ON public.data_source_oauth_bindings USING gin (source_info);
```

### Unique Indexes

```sql
CREATE UNIQUE INDEX idx_trigger_providers_endpoint 
    ON public.trigger_subscriptions USING btree (endpoint_id);
```

---

## SQL Features

### Window Functions

**ROW_NUMBER() with PARTITION BY** - Used extensively for versioning and deduplication.

```sql
SELECT *, ROW_NUMBER() OVER (PARTITION BY id ORDER BY log_version DESC) AS rn
FROM workflow_runs_log
WHERE tenant_id = :tenant_id
```

**Usage locations**:
- `core/tools/tool_manager.py` - Credential priority selection
- `extensions/logstore/repositories/*` - Log versioning queries
- `migrations/versions/*` - Data migrations

### JSONB Operators and Functions

```sql
-- Arrow operators for field access
SELECT doc_metadata->>'author' FROM documents;
SELECT doc_metadata->'nested'->>'field' FROM documents;

-- Array expansion
SELECT jsonb_array_elements_text(cast(keywords, JSONB)) FROM document_segments;

-- Path extraction (SQLAlchemy)
func.json_extract_path_text(ConversationVariable.data, "name")
```

### Aggregate Functions

```sql
-- Standard aggregates
COUNT(*), COUNT(DISTINCT column), SUM(column), AVG(column)

-- Decimal quantization in Python (post-query)
row.interactions.quantize(Decimal("0.01"))
```

### Date/Time Functions

```sql
-- Timezone conversion and date truncation
DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE :tz))

-- Epoch extraction
extract(epoch from clock_timestamp())

-- Current timestamp
CURRENT_TIMESTAMP, CURRENT_TIMESTAMP(0), clock_timestamp(), now()
```

### CASE Expressions

```sql
CASE
    WHEN SUM(provider_response_latency) = 0 THEN 0
    ELSE (SUM(answer_tokens) / SUM(provider_response_latency))
END as tokens_per_second
```

### Subqueries

```sql
SELECT AVG(subquery.message_count) AS interactions
FROM (
    SELECT conversation_id, COUNT(id) AS message_count
    FROM messages
    WHERE app_id = :app_id
    GROUP BY conversation_id
) subquery
```

### JOIN Operations

```sql
-- LEFT JOIN
SELECT m.*, mf.id AS feedback_id
FROM messages m
LEFT JOIN message_feedbacks mf ON mf.message_id = m.id AND mf.rating = 'like'

-- INNER JOIN
SELECT c.*, m.id
FROM conversations c
JOIN messages m ON c.id = m.conversation_id
```

### String Functions

```sql
-- COALESCE for null handling
COALESCE(string_agg(split_part(word, ':', 1), ' | '), '')

-- LIKE with escape (via SQLAlchemy)
column.ilike(f'%{escaped_pattern}%', escape='\\')
```

---

## DDL Patterns

### Table Creation

```sql
CREATE TABLE public.accounts (
    id uuid DEFAULT public.uuid_generate_v4() NOT NULL,
    name character varying(255) NOT NULL,
    email character varying(255) NOT NULL,
    status character varying(16) DEFAULT 'active'::character varying NOT NULL,
    created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP(0) NOT NULL,
    updated_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP(0) NOT NULL
);
```

### Default Value Patterns

```sql
-- UUID generation
DEFAULT public.uuid_generate_v4()
DEFAULT public.uuidv7()

-- Timestamps
DEFAULT CURRENT_TIMESTAMP
DEFAULT CURRENT_TIMESTAMP(0)  -- Truncated to seconds
DEFAULT now()

-- Type-cast defaults
DEFAULT 'active'::character varying
DEFAULT false
DEFAULT 0
DEFAULT '{}'::text
DEFAULT '[]'::json
```

### Constraints

```sql
-- Primary key
ALTER TABLE ONLY public.accounts ADD CONSTRAINT account_pkey PRIMARY KEY (id);

-- Unique constraints
ALTER TABLE ONLY public.accounts ADD CONSTRAINT unique_account_email UNIQUE (email);

-- Composite unique
ALTER TABLE ONLY public.tenant_account_joins 
    ADD CONSTRAINT unique_tenant_account_join UNIQUE (tenant_id, account_id);

-- Foreign key
ALTER TABLE ONLY public.tool_published_apps
    ADD CONSTRAINT tool_published_apps_app_id_fkey 
    FOREIGN KEY (app_id) REFERENCES public.apps(id);
```

### Sequences

```sql
CREATE SEQUENCE public.task_id_sequence
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

-- Usage
id integer DEFAULT nextval('public.task_id_sequence'::regclass) NOT NULL
```

### ALTER TABLE Operations

Via Alembic migrations:
```python
# Add column
batch_op.add_column(sa.Column('new_col', sa.String(255), server_default=sa.text("'default'")))

# Modify column
batch_op.alter_column('col', existing_type=sa.String(255), type_=sa.String(512))

# Add index
op.create_index('idx_name', 'table_name', ['col1', 'col2'])
```

---

## DML Patterns

### Parameterized Queries

All queries use parameterized statements (`:param` syntax):

```sql
SELECT * FROM messages 
WHERE app_id = :app_id 
  AND created_at >= :start 
  AND created_at < :end
GROUP BY date 
ORDER BY date
```

### Bulk Operations

```sql
-- Batch delete
DELETE FROM embeddings WHERE id = :embedding_id

-- Delete with IN clause
DELETE FROM workflow_runs WHERE id IN :run_ids
```

### UPDATE with Returning

```python
# SQLAlchemy pattern
stmt = update(Model).where(...).values(...).returning(Model.id)
result = session.execute(stmt)
```

### INSERT Patterns

Primarily through SQLAlchemy ORM:
```python
session.add(new_object)
session.commit()
```

---

## Connection & Transaction Patterns

### Connection Pool Configuration

From SQLAlchemy/Flask-SQLAlchemy:
- `pool_size` - Number of persistent connections
- `max_overflow` - Additional connections allowed
- `pool_pre_ping` - Verify connection liveness
- `pool_recycle` - Connection lifetime

### Transaction Management

```python
# Context manager pattern (recommended)
with Session(db.engine, expire_on_commit=False) as session:
    stmt = select(Model).where(...)
    result = session.execute(stmt).scalar_one_or_none()

# Flask-SQLAlchemy pattern
with db.engine.begin() as conn:
    rs = conn.execute(sa.text(sql_query), params)
```

### Session Scoping

- Sessions are request-scoped in Flask
- `expire_on_commit=False` for detached object access
- Gevent compatibility with connection pool reset handling

---

## Compatibility Checklist

### Critical (Must Have)

- [ ] **uuid-ossp extension** or equivalent UUID generation
- [ ] **UUID data type** with proper comparison/sorting
- [ ] **JSONB data type** with GIN index support
- [ ] **JSONB operators**: `->`, `->>`, `@>`
- [ ] **JSONB functions**: `jsonb_array_elements_text()`
- [ ] **Window functions**: `ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...)`
- [ ] **Date/time functions**: `DATE_TRUNC()`, `AT TIME ZONE`, `extract(epoch from ...)`
- [ ] **Aggregate functions**: `COUNT`, `SUM`, `AVG`, `COUNT(DISTINCT ...)`
- [ ] **B-tree indexes** with composite columns and DESC ordering
- [ ] **GIN indexes** on JSONB columns
- [ ] **Sequences** with `nextval()`
- [ ] **Timestamp precision**: `CURRENT_TIMESTAMP(0)`

### Important (Required for Full Functionality)

- [ ] **Bit manipulation**: `set_bit()`, `overlay()` on bytea
- [ ] **Binary functions**: `encode()`, `int8send()`, `uuid_send()`
- [ ] **clock_timestamp()** (differs from `now()` within transaction)
- [ ] **gen_random_uuid()** (built-in random UUID)
- [ ] **Type casting**: `::character varying`, `::uuid`, `::bytea`
- [ ] **CASE expressions**
- [ ] **Subqueries** in FROM clause
- [ ] **LEFT JOIN / INNER JOIN**
- [ ] **COALESCE**
- [ ] **String functions**: `string_agg()`, `split_part()`

### Nice to Have

- [ ] **COMMENT ON** for documentation
- [ ] **Parallel query** support (`PARALLEL SAFE` functions)
- [ ] **pg_dump/pg_restore** compatibility

---

## Test Queries

### UUID Generation

```sql
SELECT uuid_generate_v4();
SELECT public.uuidv7();
```

### JSONB Operations

```sql
CREATE TABLE test_jsonb (id uuid PRIMARY KEY, data jsonb);
INSERT INTO test_jsonb VALUES (uuid_generate_v4(), '{"name": "test", "tags": ["a", "b"]}');

-- Query
SELECT data->>'name' FROM test_jsonb;
SELECT jsonb_array_elements_text(data->'tags') FROM test_jsonb;

-- GIN index
CREATE INDEX ON test_jsonb USING gin (data);
SELECT * FROM test_jsonb WHERE data @> '{"name": "test"}';
```

### Window Functions

```sql
SELECT id, created_at, 
       ROW_NUMBER() OVER (PARTITION BY tenant_id ORDER BY created_at DESC) as rn
FROM workflow_runs
WHERE rn = 1;
```

### Timezone Conversion

```sql
SELECT DATE(DATE_TRUNC('day', 
    '2024-01-15 10:30:00'::timestamp AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'
));
```

---

## Version History

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | 2025-01-20 | Initial specification based on Dify codebase analysis |

# Protocol Module

PostgreSQL wire protocol via pgwire.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `handler/dynamic.rs` | ~2000 | pgwire handlers, type mapping, on_parse analysis |
| `handler/mod.rs` | ~280 | Exports, utility functions |
| `handler/params/` | | Parameter counting + decoding |
| `handler/encode/` | | Value encoding, type mapping |

## Handlers

- `DynamicPgHandler` - Production handler (per-connection TiKV client)
- `DynamicHandlerFactory` - Creates handlers with keyspace from username

## Protocol Flows

**Simple Query:**
```
Query("SELECT ...") → do_query() → executor.execute() → result_to_response()
```

**Extended Query (ORMs):**
```
Parse → Analyze → Bind → Describe → Execute
                           ↓
             AnalyzedQuery output_schema → PostgreSQL OIDs
```

For data statements (SELECT/INSERT/UPDATE/DELETE), `on_parse` runs the Analyzer
to produce typed IR with `output_schema`. Describe reads this schema directly.

For utility statements (DDL/SET/SHOW), `on_parse` keeps `RawSqlUtility` and
Describe uses `utility_describe_fields()` for static schema mapping.

## Where to Look

| Task | Location |
|------|----------|
| Add PostgreSQL type | `encode/types.rs` → `datatype_to_pgtype()` |
| Fix type OID | Analyzer `output_schema` (analyzer/query.rs) |
| Change value encoding | `encode/result.rs` → `encode_value()` |
| Fix parameter decoding | `params/decode.rs` → `decode_parameters()` |

## Key Functions

```
count_sql_parameters()       # Count $N placeholders in SQL
decode_parameters()          # Wire bytes → Value (Bind phase)
utility_describe_fields()    # Static Describe for SHOW/EXPLAIN
is_data_statement()          # AST-based SELECT/DML classification
reject_unanalyzed_if_needed()# Guard: reject unanalyzed data SQL
result_to_response()         # ExecuteResult → pgwire Response
datatype_to_pgtype()         # Internal type → pg Type
parse_tenant_username()      # "tenant.user" → (keyspace, user)
```

## PostgreSQL Type OIDs

| Internal | PostgreSQL | OID |
|----------|------------|-----|
| Int32 | INT4 | 23 |
| Int64 | INT8 | 20 |
| Float64 | FLOAT8 | 701 |
| Boolean | BOOL | 16 |
| Text | TEXT | 25 |
| Timestamp | TIMESTAMPTZ | 1184 |
| Uuid | UUID | 2950 |
| Jsonb | JSONB | 3802 |

## Multi-Tenancy

Username format: `tenant.user` or `tenant:user`
- Extracts keyspace from username
- Each keyspace gets isolated TiKV client
- Pooled in `src/pool.rs`

## Common Issues

**Wrong OIDs (integers as strings):**
- Check Analyzer `output_schema` for correct DataType
- Check `column_types` in executor result

**Describe returns empty for SHOW/EXPLAIN:**
- Check `utility_describe_fields()` in dynamic.rs

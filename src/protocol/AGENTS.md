# Protocol Module

PostgreSQL wire protocol via pgwire. ~2300 lines.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `handler.rs` | 2250 | pgwire handlers, type mapping |
| `mod.rs` | 50 | Exports |

## Handlers

- `DynamicPgHandler` - Production handler (per-connection TiKV client)
- `DynamicHandlerFactory` - Creates handlers with keyspace from username
- `PgHandler` - Legacy static handler (deprecated, kept for compatibility)

## Protocol Flows

**Simple Query:**
```
Query("SELECT ...") → do_query() → executor.execute() → result_to_response()
```

**Extended Query (ORMs):**
```
Parse → Bind → Describe → Execute
                  ↓
        infer_result_fields_from_query() → PostgreSQL OIDs
```

## Where to Look

| Task | Location |
|------|----------|
| Add PostgreSQL type | `datatype_to_pgtype()` |
| Fix type OID | `infer_result_fields_from_query()` |
| Change value encoding | `encode_value()` |
| Fix RETURNING types | `find_keyword_outside_strings()` |

## Key Functions

```
find_keyword_outside_strings()    # Find keyword not in string literals
infer_result_fields_from_query()  # Column type inference for Describe
substitute_parameters()           # Replace $1, $2 with values
result_to_response()              # ExecuteResult → pgwire Response
datatype_to_pgtype()              # Internal type → pg Type
parse_tenant_username()           # "tenant.user" → (keyspace, user)
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
- Check `infer_result_fields_from_query()`
- Check `column_types` in executor result

**RETURNING parse errors:**
- Use `find_keyword_outside_strings()` for keyword search
- Handle quoted table names

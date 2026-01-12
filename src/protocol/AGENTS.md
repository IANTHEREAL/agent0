# Protocol Module

PostgreSQL wire protocol implementation using pgwire. ~2300 lines.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `handler.rs` | 2246 | pgwire handlers, query execution, type mapping |
| `mod.rs` | ~50 | Module exports |

## Key Components

### Handlers
- `DynamicPgHandler` - Main handler (used in production)
- `PgHandler` - Legacy static handler
- `DynamicHandlerFactory` - Creates handlers per connection

### Protocol Flows

**Simple Query Protocol:**
```
Client → Query("SELECT ...") 
      → do_query() 
      → executor.execute() 
      → result_to_response()
```

**Extended Query Protocol (ORMs use this):**
```
Client → Parse("SELECT ... $1")     → stores prepared statement
      → Bind(params)                → creates portal with values
      → Describe                    → infer_result_fields_from_query() → OIDs
      → Execute                     → do_query() → result_to_response()
```

## Key Functions

### handler.rs
```
find_keyword_outside_strings()    # ~line 80  - Find SQL keyword not in strings
infer_result_fields_from_query()  # ~line 126 - Infer column types for Describe
substitute_parameters()           # ~line 1015 - Replace $1, $2 with values
result_to_response()              # ~line 1723 - Convert ExecuteResult to pgwire Response
datatype_to_pgtype()              # ~line 1705 - Map internal DataType to pg Type
encode_value()                    # Encode Value to wire format
```

### Type Inference (Critical for ORMs)

The `infer_result_fields_from_query()` function handles type inference:

1. **For RETURNING clauses**: Parses table name, looks up schema, maps column types
2. **For SELECT queries**: Executes with LIMIT 1, uses `column_types` from result

**Important**: When parsing RETURNING, must handle:
- Quoted table names: `"table_name"`
- Column lists: `INSERT INTO table(col1, col2)` → extract just `table`
- Keywords in strings: `'returning@example.com'` must not match RETURNING keyword

### PostgreSQL Type OIDs

| Internal Type | PostgreSQL Type | OID |
|---------------|-----------------|-----|
| Int32 | INT4 | 23 |
| Int64 | INT8 | 20 |
| Float64 | FLOAT8 | 701 |
| Boolean | BOOL | 16 |
| Text | TEXT | 25 |
| Timestamp | TIMESTAMPTZ | 1184 |
| Uuid | UUID | 2950 |
| Jsonb | JSONB | 3802 |

## Common Issues

### Type Mismatch (integers returned as strings)
- **Symptom**: OID 25 (TEXT) instead of 23 (INT4)
- **Cause**: Type inference failed, defaulted to TEXT
- **Fix**: Check `infer_result_fields_from_query()` and `column_types` in executor

### RETURNING parsing errors
- **Symptom**: Wrong columns returned or type errors
- **Cause**: Table name extraction failed or keyword found in string
- **Fix**: Use `find_keyword_outside_strings()` for keyword search

## Adding Support for New Types

1. Add variant to `datatype_to_pgtype()`:
```rust
Some(DataType::MyType) => Type::MY_PG_TYPE,
```

2. Add encoding in `encode_value()`:
```rust
Value::MyType(v) => encoder.encode_field(&v),
```

3. Update `infer_result_fields_from_query()` type mapping if needed.

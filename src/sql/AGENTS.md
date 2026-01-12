# SQL Module

Core SQL parsing and execution. ~15,000 lines across 15 files.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `executor.rs` | 3450 | Main query execution, SELECT with JOIN/WHERE/GROUP BY |
| `expr.rs` | 3652 | Expression evaluation, functions, operators |
| `helpers.rs` | 1097 | Utility functions, type inference, row helpers |
| `information_schema.rs` | 1295 | Virtual tables for schema introspection |
| `window.rs` | 1025 | Window function evaluation (ROW_NUMBER, RANK, etc.) |
| `dml.rs` | 991 | INSERT, UPDATE, DELETE with RETURNING |
| `ddl.rs` | 922 | CREATE/DROP TABLE, INDEX, VIEW |
| `planner.rs` | 823 | Query optimization, index scan selection |
| `explain.rs` | 692 | EXPLAIN query plans |
| `aggregate.rs` | 341 | COUNT, SUM, AVG, MIN, MAX, STRING_AGG |
| `rbac.rs` | 281 | GRANT/REVOKE, role-based access control |
| `query.rs` | 166 | Query helper types |
| `result.rs` | 111 | ExecuteResult enum |
| `session.rs` | 106 | Transaction state, current user |
| `parser.rs` | 88 | sqlparser wrapper |
| `mod.rs` | 23 | Module exports |

## Where to Look

| Task | Location |
|------|----------|
| Add function (UPPER, NOW, etc.) | `expr.rs` → `eval_function()` |
| Add operator (+, -, LIKE, etc.) | `expr.rs` → `eval_binary_op()` |
| Add statement (CREATE, DROP) | `executor.rs` → `execute_statement_on_txn()` |
| Add aggregate | `aggregate.rs` → `Aggregator` enum |
| Add window function | `window.rs` → `compute_window_functions()` |
| Change transaction behavior | `session.rs` |
| Fix type inference | `helpers.rs` → `infer_expr_type()` |
| Add information_schema table | `information_schema.rs` |

## Key Functions

### helpers.rs
```
infer_expr_type()        # Infer DataType from SQL expression + schema
get_select_item_name()   # Extract column name/alias from SelectItem
sql_datatype_to_internal() # Convert sqlparser DataType to internal
dedup_rows()             # Deduplicate rows for DISTINCT
distinct_on_rows()       # PostgreSQL DISTINCT ON
fill_row_defaults()      # Fill missing columns with DEFAULT values
```

### expr.rs
```
eval_function()      # ~line 400  - String/math/date functions
eval_binary_op()     # ~line 200  - Operators (+, -, *, /, LIKE)
eval_json_op()       # ~line 600  - JSON ->, ->> operators
eval_cast()          # ~line 800  - Type casts
```

### executor.rs
```
execute_select()         # Main SELECT with JOIN, WHERE, GROUP BY
execute_query_with_ctes() # CTEs (WITH ... AS)
column_types extraction  # ~line 2050 - Uses infer_expr_type for proper OIDs
```

### dml.rs
```
execute_insert()     # INSERT with RETURNING, ON CONFLICT
execute_update()     # UPDATE with RETURNING
execute_delete()     # DELETE with RETURNING
build_returning_columns() # Extract RETURNING column list
```

## Adding a SQL Function

1. Add match arm in `expr.rs` → `eval_function()`:
```rust
"my_func" => {
    let arg = eval_expr(args[0], row, schema)?;
    // transform arg
    Ok(Value::Text(result))
}
```

2. Add type inference in `helpers.rs` → `infer_expr_type()` if needed:
```rust
"MY_FUNC" => DataType::Text,  // Return type of the function
```

3. Add test in same file or `tests/` directory.

## Type Inference Flow

When TypeORM/ORMs use column aliases like `SELECT id AS "User_id"`:

1. `executor.rs` builds `column_types` using `infer_expr_type(expr, schema)`
2. `infer_expr_type` looks at the **expression** (not alias) to determine type
3. For `id AS "User_id"`, it looks up `id` in schema → returns `Int32`
4. Protocol handler uses these types to set correct PostgreSQL OIDs

## Known Issues

- `executor.rs` and `expr.rs` are large → consider further splitting
- `eval_expr` vs `eval_expr_join` duplication (single table vs JOIN context)
- Window functions sort entire result set (not streaming)
- information_schema support is incomplete

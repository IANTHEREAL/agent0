# SQL Module

Core SQL parsing and execution. ~15,000 lines across 16 files.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `executor.rs` | 3500 | Main query execution, SELECT with JOIN/WHERE/GROUP BY |
| `expr.rs` | 3700 | Expression evaluation, functions, operators |
| `helpers.rs` | 1100 | Utility functions, type inference |
| `information_schema.rs` | 1300 | Virtual tables (pg_catalog, information_schema) |
| `window.rs` | 1025 | Window functions (ROW_NUMBER, RANK, LAG/LEAD) |
| `dml.rs` | 1000 | INSERT, UPDATE, DELETE with RETURNING |
| `ddl.rs` | 950 | CREATE/DROP TABLE, INDEX, VIEW |
| `planner.rs` | 850 | Query optimization, index selection |
| `explain.rs` | 700 | EXPLAIN query plans |
| `aggregate.rs` | 350 | COUNT, SUM, AVG, MIN, MAX, STRING_AGG |
| `query.rs` | 170 | Set operations (UNION, INTERSECT, EXCEPT) |
| `result.rs` | 112 | ExecuteResult enum |
| `session.rs` | 107 | Transaction state |
| `parser.rs` | 88 | sqlparser wrapper |

## Where to Look

| Task | Location |
|------|----------|
| Add function | `expr.rs` → `eval_function()` |
| Add operator | `expr.rs` → `eval_binary_op()` |
| Add statement | `executor.rs` → `execute_statement_on_txn()` |
| Add aggregate | `aggregate.rs` → `Aggregator` enum |
| Add window function | `window.rs` → `compute_window_functions()` |
| Fix type inference | `helpers.rs` → `infer_expr_type()` |

## Key Functions

### expr.rs
```
eval_function()      # String/math/date functions (~line 400)
eval_binary_op()     # Operators: +, -, *, /, LIKE (~line 200)
eval_json_op()       # JSON ->, ->> operators (~line 600)
eval_cast()          # Type casts (~line 800)
compare_values()     # Value comparison for ORDER BY
```

### helpers.rs
```
infer_expr_type()        # Infer DataType from expression
get_select_item_name()   # Extract column name/alias
sql_datatype_to_internal() # sqlparser DataType → internal
fill_row_defaults()      # Fill DEFAULT values
```

### executor.rs
```
execute_select()           # Main SELECT logic
execute_query_with_ctes()  # WITH ... AS (CTEs)
execute_join_query_with_ctes() # JOIN execution
```

## Adding a Function

1. Add match in `expr.rs` → `eval_function()`:
```rust
"my_func" => {
    let arg = eval_expr(&args[0], row, schema)?;
    Ok(Value::Text(transform(arg)))
}
```

2. Add type inference in `helpers.rs` → `infer_expr_type()`:
```rust
"MY_FUNC" => DataType::Text,
```

## Known Issues

- `executor.rs` and `expr.rs` are large → consider splitting
- `eval_expr` vs `eval_expr_join` duplication
- Window functions materialize entire result set

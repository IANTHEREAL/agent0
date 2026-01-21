# PRD-004: pg_typeof()

## Summary
Implement `pg_typeof(expression)` function to return the data type of any expression.

## Motivation
- Debugging and introspection
- ORMs use for schema validation
- Useful for understanding type coercion behavior

## Requirements

### Functional
```sql
SELECT pg_typeof(1);           -- integer
SELECT pg_typeof(1.5);         -- numeric
SELECT pg_typeof('hello');     -- text
SELECT pg_typeof(true);        -- boolean
SELECT pg_typeof(NULL);        -- unknown
SELECT pg_typeof(NOW());       -- timestamp with time zone
SELECT pg_typeof(ARRAY[1,2]);  -- integer[]
SELECT pg_typeof('{"a":1}'::jsonb);  -- jsonb
```

### Return Type
- Returns `regtype` (text representation of type OID)
- Use lowercase PostgreSQL type names

## Acceptance Criteria
```sql
SELECT pg_typeof(42);
-- Returns: 'integer'

SELECT pg_typeof(3.14::numeric);
-- Returns: 'numeric'

SELECT pg_typeof(NULL::text);
-- Returns: 'text'
```

## Implementation Notes

### Type Mapping
| Internal Type | pg_typeof Result |
|---------------|------------------|
| `Value::Int32` | `integer` |
| `Value::Int64` | `bigint` |
| `Value::Float64` | `double precision` |
| `Value::Numeric` | `numeric` |
| `Value::Text` | `text` |
| `Value::Boolean` | `boolean` |
| `Value::Timestamp` | `timestamp without time zone` |
| `Value::TimestampTz` | `timestamp with time zone` |
| `Value::Date` | `date` |
| `Value::Uuid` | `uuid` |
| `Value::Json` | `json` |
| `Value::Jsonb` | `jsonb` |
| `Value::Bytea` | `bytea` |
| `Value::Array(_)` | `<element_type>[]` |
| `Value::Null` | `unknown` |

### Implementation
```rust
"pg_typeof" => {
    let val = eval_expr(&args[0], row, schema)?;
    Ok(Value::Text(val.pg_type_name().into()))
}
```

Add `pg_type_name()` method to `Value` enum.

## Effort
~2 hours

## Test File
`tests/73_system_functions.sql` (extend existing)

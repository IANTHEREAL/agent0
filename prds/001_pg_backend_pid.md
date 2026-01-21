# PRD-001: pg_backend_pid()

## Summary
Implement `pg_backend_pid()` function to return a unique connection identifier.

## Motivation
Many ORMs and tools call `pg_backend_pid()` for:
- Connection health checks
- Logging/debugging (correlate queries to connections)
- Advisory lock namespacing

Currently returns error, breaking compatibility.

## Requirements

### Functional
- `SELECT pg_backend_pid()` returns an INT4 unique to the current connection
- Value remains stable for the lifetime of the connection
- Different connections get different PIDs

### Non-functional
- No TiKV round-trip (pure in-memory)
- No cross-node coordination needed

## Acceptance Criteria
```sql
SELECT pg_backend_pid();
-- Returns: integer (e.g., 12345)

SELECT pg_backend_pid() = pg_backend_pid();
-- Returns: true (stable within session)
```

## Implementation Notes
- Generate connection ID in `DynamicPgHandler::new()` (atomic counter or random)
- Store in handler struct, pass to executor context
- Add match in `expr.rs` → `eval_function()`:
  ```rust
  "pg_backend_pid" => Ok(Value::Int32(session.connection_id()))
  ```

## Effort
~2 hours

## Test File
`tests/73_system_functions.sql` (extend existing)

# PRD-003: version()

## Summary
Implement `version()` function to return a PostgreSQL-compatible version string.

## Motivation
- Every PostgreSQL driver calls `version()` on connect
- Used for feature detection and compatibility checks
- Required by pg_dump, psql, and most admin tools

## Requirements

### Functional
```sql
SELECT version();
-- Returns: 'PostgreSQL 16.0 (db9-server 0.1.0 on TiKV)'
```

Format: `PostgreSQL <major>.<minor> (db9-server <version> on TiKV)`

### Non-functional
- Compile-time constant (from Cargo.toml version)
- No runtime overhead

## Acceptance Criteria
```sql
SELECT version();
-- Contains 'PostgreSQL'
-- Contains 'db9-server'

SELECT version() LIKE 'PostgreSQL%';
-- Returns: true
```

## Implementation Notes
- Add to `expr.rs` → `eval_function()`:
  ```rust
  "version" => {
      let ver = format!(
          "PostgreSQL 16.0 (db9-server {} on TiKV)",
          env!("CARGO_PKG_VERSION")
      );
      Ok(Value::Text(ver))
  }
  ```

## Effort
~1 hour

## Test File
`tests/73_system_functions.sql` (extend existing)

# PRD-002: current_setting()

## Summary
Implement `current_setting(name)` function to return PostgreSQL configuration values.

## Motivation
ORMs and drivers call `current_setting()` for:
- Version detection (`server_version`, `server_version_num`)
- Feature detection (`standard_conforming_strings`)
- Timezone handling (`TimeZone`)

## Requirements

### Functional
Support these commonly-queried settings:

| Setting | Return Value |
|---------|--------------|
| `server_version` | `16.0` (or db9-server version) |
| `server_version_num` | `160000` |
| `standard_conforming_strings` | `on` |
| `client_encoding` | `UTF8` |
| `TimeZone` | `UTC` |
| `search_path` | Current search_path |
| `application_name` | Connection app name |

### Behavior
- `current_setting('unknown')` → error (matches PostgreSQL)
- `current_setting('unknown', true)` → NULL (missing_ok parameter)

## Acceptance Criteria
```sql
SELECT current_setting('server_version');
-- Returns: '16.0'

SELECT current_setting('nonexistent', true);
-- Returns: NULL

SELECT current_setting('nonexistent');
-- ERROR: unrecognized configuration parameter
```

## Implementation Notes
- Add to `expr.rs` → `eval_function()`:
  ```rust
  "current_setting" => {
      let name = args[0].as_str()?;
      let missing_ok = args.get(1).map(|v| v.as_bool()).unwrap_or(false);
      match name {
          "server_version" => Ok(Value::Text("16.0".into())),
          // ...
          _ if missing_ok => Ok(Value::Null),
          _ => Err(anyhow!("unrecognized configuration parameter: {}", name)),
      }
  }
  ```

## Effort
~3 hours

## Test File
`tests/73_system_functions.sql` (extend existing)

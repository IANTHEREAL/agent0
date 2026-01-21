# pg-tikv PRDs

Product Requirements Documents for planned features.

## Quick Wins (< 1 day each)

| PRD | Feature | Effort | Status |
|-----|---------|--------|--------|
| [001](001_pg_backend_pid.md) | `pg_backend_pid()` | 2h | Planned |
| [002](002_current_setting.md) | `current_setting()` | 3h | Planned |
| [003](003_version_function.md) | `version()` | 1h | Planned |
| [004](004_pg_typeof.md) | `pg_typeof()` | 2h | Planned |

**Total: ~8 hours**

## Implementation Order

1. **version()** - simplest, immediate ORM compatibility boost
2. **pg_backend_pid()** - connection health checks
3. **current_setting()** - feature detection
4. **pg_typeof()** - debugging/introspection

## Future PRDs

- Partial indexes (`CREATE INDEX ... WHERE`)
- Expression indexes (`CREATE INDEX ON t (LOWER(col))`)
- COPY TO stdout
- GIN indexes for JSONB

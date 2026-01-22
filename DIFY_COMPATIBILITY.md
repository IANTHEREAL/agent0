# Dify Compatibility Report for pg-tikv

This document tracks compatibility issues found when running [Dify](https://github.com/langgenius/dify) with pg-tikv as the PostgreSQL backend.

## Test Environment

- **Dify Version**: 1.11.4 (from docker-compose.yaml)
- **pg-tikv**: Current development version
- **Test Date**: 2026-01-22

## Configuration

```bash
# pg-tikv running on
PG_HOST=0.0.0.0
PG_PORT=5433
PD_ENDPOINTS=127.0.0.1:45959

# Dify .env configuration
DB_HOST=10.0.0.164  # Host machine IP (not host.docker.internal on Linux)
DB_PORT=5433
DB_USERNAME=admin
DB_PASSWORD=admin
DB_DATABASE=postgres
```

## Test Results Summary

### ✅ Working

1. **Database Connection** - Dify can connect to pg-tikv
2. **Database Migrations** - Alembic migrations complete successfully (123 tables created)
3. **Table Creation** - All Dify tables created
4. **Plugin Daemon** - Successfully initializes database
5. **Web Interface** - Install page accessible at http://localhost:8088/install
6. **COMMENT ON COLUMN** - Now works correctly (fixed 2026-01-22)

### ⚠️ Known Issues

See detailed issues below.

---

## Fixed Issues

### Issue: COMMENT ON COLUMN returns EmptyQueryResponse

**Status**: ✅ Fixed (2026-01-22)

**Error Message**:
```
psycopg2.ProgrammingError: can't execute an empty query
```

**Root Cause**:
- `COMMENT ON COLUMN` was returning `ExecuteResult::Empty` which mapped to `Response::EmptyQuery`
- PostgreSQL clients (psycopg2/libpq) treat `EmptyQueryResponse` as an error for utility statements

**Solution**:
Changed `execute_comment_on_cmd()` to return `ExecuteResult::CommandComplete { tag: "COMMENT" }` instead of `ExecuteResult::Empty`.

**Files Modified**:
- `src/sql/executor.rs` line ~1416

---

## Remaining Issues

### Issue 1: `pg_get_userbyid` function not supported

**Status**: 🔴 Not Fixed

**Error Message**:
```
ERROR:  Unsupported function in JOIN: pg_get_userbyid
```

**Context**:
- Occurs when using `psql \dt` command (list tables)
- The `\dt` meta-command queries `pg_catalog` views that use `pg_get_userbyid()`

**Impact**: Low - This is a psql convenience function, not used by application code

**Workaround**:
```sql
-- Instead of \dt, use:
SELECT table_name FROM information_schema.tables WHERE table_schema = 'public';
```

**Fix Plan**:
- [ ] Implement `pg_get_userbyid(oid)` function in `src/sql/expr.rs`
- [ ] Should return username for given OID, or 'unknown' if not found

**Files to Modify**:
- `src/sql/expr.rs` → `eval_function()`

---

## Testing Progress

- [x] Dify docker-compose starts
- [x] pg-tikv accepts connections from Docker containers
- [x] Database migrations complete (123 tables)
- [x] Plugin daemon initializes
- [x] Web interface loads
- [x] User registration (via `/console/api/setup`)
- [x] Workspace creation (automatic with user registration)
- [ ] Create application
- [ ] LLM integration
- [ ] Full workflow execution

### Verified Database Records

After setup, the following records were created successfully:
- `accounts` table: Admin user with email `admin@example.com`
- `tenants` table: Workspace "Admin's Workspace" with `basic` plan
- `dify_setups` table: Version `1.11.4` recorded

---

## Setup Instructions

### Quick Start

```bash
# 1. Start TiKV cluster (if not already running)
cd ~/lab/pg-tikv
uv run scripts/tikv_admin.py start --name dify-test --persistent

# 2. Start pg-tikv
PD_ENDPOINTS=127.0.0.1:<pd_port> PG_PORT=5433 PG_HOST=0.0.0.0 ./target/release/pg-tikv

# 3. Configure and start Dify
cd ~/lab/dify/docker
# Edit .env:
#   DB_HOST=<your-host-ip>  (use `ip route get 1 | awk '{print $7}'` to find)
#   DB_PORT=5433
#   DB_USERNAME=admin
#   DB_PASSWORD=admin
#   COMPOSE_PROFILES=weaviate  (remove postgresql profile)
#   EXPOSE_NGINX_PORT=8088     (if port 80 is in use)

docker compose up -d

# 4. Access Dify
open http://localhost:8088/install
```

### Using Test Script

```bash
cd ~/lab/pg-tikv
./scripts/dify_test.sh start    # Start everything
./scripts/dify_test.sh logs     # View logs
./scripts/dify_test.sh status   # Check status
./scripts/dify_test.sh stop     # Stop Dify
```

---

## Summary

| Category | Count |
|----------|-------|
| Critical (Blocking) | 0 |
| High | 0 |
| Medium | 0 |
| Low | 1 |
| **Total** | **1** |

---

*Last Updated: 2026-01-22*

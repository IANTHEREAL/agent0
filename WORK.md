# Work Log: Dify Compatibility Testing

## 2026-01-22: Dify Compatibility Testing Complete

### Goal
Test pg-tikv compatibility with Dify (https://github.com/langgenius/dify), a popular LLM application platform that uses PostgreSQL as its metadata database.

### Final Status: SUCCESS

Dify is now running successfully with pg-tikv as the database backend.

### Summary of Work

#### 1. Configuration Changes

**Dify `.env` modifications (`~/lab/dify/docker/.env`):**
```
DB_HOST=10.0.0.164          # Host machine IP (host.docker.internal doesn't work on Linux)
DB_PORT=5433
DB_USERNAME=admin
DB_PASSWORD=admin
DB_DATABASE=postgres
COMPOSE_PROFILES=weaviate   # Removed postgresql profile to skip built-in Postgres
EXPOSE_NGINX_PORT=8088      # Changed from 80 (port conflict)
EXPOSE_NGINX_SSL_PORT=8443  # Changed from 443 (port conflict)
```

**pg-tikv startup:**
```bash
PD_ENDPOINTS=127.0.0.1:45959 PG_PORT=5433 PG_HOST=0.0.0.0 ./target/release/pg-tikv
```

#### 2. Bug Found and Fixed

**Issue:** `COMMENT ON COLUMN` returns `EmptyQueryResponse` instead of `CommandComplete`

**Symptom:** Alembic migrations fail silently after executing `COMMENT ON COLUMN` statements. The psycopg2 driver throws "can't execute an empty query" error.

**Root Cause:** In `src/sql/executor.rs`, the `execute_comment_on_cmd()` function returned `ExecuteResult::Empty`, which mapped to `Response::EmptyQuery` in the wire protocol handler. PostgreSQL clients (psycopg2/libpq) treat `EmptyQueryResponse` as an error for utility statements.

**Fix:** Changed line ~1416 in `src/sql/executor.rs`:
```rust
// Before
Ok(ExecuteResult::Empty)

// After  
Ok(ExecuteResult::CommandComplete { tag: "COMMENT" })
```

**Verification:**
- All 410 unit tests pass
- Dify migrations complete successfully (123 tables created)
- User registration and workspace creation work correctly

#### 3. Test Results

| Test | Status |
|------|--------|
| Database connection from Docker | ✅ Pass |
| Alembic migrations (123 tables) | ✅ Pass |
| Plugin daemon initialization | ✅ Pass |
| Web interface loads | ✅ Pass |
| User registration via API | ✅ Pass |
| Workspace creation | ✅ Pass |
| Database queries (SELECT, INSERT, UPDATE) | ✅ Pass |
| Transactions | ✅ Pass |

#### 4. Remaining Known Issues

**`pg_get_userbyid` function not supported** (Low priority)
- Only affects `psql \dt` command
- Not used by Dify application code
- Workaround: Use `SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'`

### Files Created/Modified

| File | Description |
|------|-------------|
| `src/sql/executor.rs` | Fixed COMMENT ON return value |
| `DIFY_COMPATIBILITY.md` | Compatibility documentation |
| `WORK.md` | This work log |
| `scripts/dify_test.sh` | Test script for Dify deployment |

### How to Reproduce

```bash
# 1. Start TiKV cluster
cd ~/lab/pg-tikv
uv run scripts/tikv_admin.py start --name dify-test --persistent
# Note the PD port from output

# 2. Build and start pg-tikv
cargo build --release
PD_ENDPOINTS=127.0.0.1:<pd_port> PG_PORT=5433 PG_HOST=0.0.0.0 ./target/release/pg-tikv

# 3. Configure Dify
cd ~/lab/dify/docker
# Edit .env with DB_HOST=<your-ip>, DB_PORT=5433, etc.
# Set COMPOSE_PROFILES=weaviate to skip built-in PostgreSQL

# 4. Start Dify
docker compose up -d

# 5. Access Dify
open http://localhost:8088/install
```

### Lessons Learned

1. **Wire protocol matters**: Utility statements (BEGIN, COMMIT, SET, COMMENT, etc.) must return `CommandComplete` or appropriate response, never `EmptyQueryResponse`. Clients like psycopg2 treat empty responses as errors.

2. **Testing with real ORMs is essential**: The unit tests didn't catch this issue because they don't test the wire protocol responses. Testing with actual PostgreSQL drivers (psycopg2, pg, etc.) reveals wire-level compatibility issues.

3. **Docker networking on Linux**: `host.docker.internal` doesn't work by default on Linux Docker. Use the actual host IP address instead.

---

*Completed: 2026-01-22*

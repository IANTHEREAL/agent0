# External Integrations

**Analysis Date:** 2026-03-17

## APIs & External Services

**Vector Embedding:**
- OpenAI-compatible embedding API (default: Alibaba Cloud DashScope at `https://dashscope-intl.aliyuncs.com/compatible-mode/v1/embeddings`)
  - SDK/Client: `reqwest` 0.12 (custom HTTP client in `src/extensions/embedding.rs`)
  - Auth: `EMBEDDING_API_KEY` env var
  - Config: `EMBEDDING_ENDPOINT` or `EMBEDDING_BASE_URL`, `EMBEDDING_MODEL` (default `text-embedding-v4`), `EMBEDDING_DIMENSIONS` (default 1024)
  - Provider type: `EMBEDDING_PROVIDER` env var — `openai` (default) or `bedrock`

- AWS Bedrock embedding endpoint
  - SDK/Client: `reqwest` 0.12 (same client as OpenAI-compatible path)
  - Auth: AWS SDK credential chain (`aws-config`)
  - Config: `EMBEDDING_PROVIDER=bedrock`, `EMBEDDING_ENDPOINT` (Bedrock invoke URL)

**HTTP Extension (SQL-callable):**
- Arbitrary outbound HTTP via SQL functions `http_get()`, `http_post()`, `http_put()`, `http_delete()`, `http_head()`, `http()`
  - Implementation: `src/extensions/http.rs`
  - Client: `reqwest` 0.12, per-tenant concurrency limit (20 in-flight, 5 reserved for interactive)
  - Limits: 100 requests/statement, 1 MB response, 256 KB request, 3 redirects, 5s timeout
  - Security: HTTPS-only by default; `DB9_HTTP_ALLOW_INSECURE=1` to allow HTTP

**JWT / JWKS Auth:**
- External JWKS endpoint (any OIDC-compatible identity provider)
  - SDK/Client: `reqwest` 0.12 + `jsonwebtoken` 9
  - Config: `DB9_AUTH_JWKS_URL` (fetches and caches JWKS, 60s TTL)
  - Alt: `DB9_AUTH_JWT_PUBLIC_KEY` (static PEM public key)
  - Params: `DB9_AUTH_ISSUER`, `DB9_AUTH_AUDIENCE`, `DB9_AUTH_JWT_ALGORITHM`
  - Implementation: `src/auth/db9_auth.rs`

**Connect Key Introspection:**
- External API to validate `db9ck_*` connect keys
  - Config: `DB9_AUTH_CONNECT_KEY_INTROSPECT_URL`, `DB9_AUTH_CONNECT_KEY_INTROSPECT_API_KEY`
  - Implementation: `src/auth/db9_auth.rs`

## Data Storage

**Databases:**
- TiKV (distributed key-value store) — primary persistent storage
  - Connection: `PD_ENDPOINTS` env var (PD placement driver endpoints, default `127.0.0.1:2379`)
  - Client: `tikv-client` (vendored at `vendor/tikv-client`; customized for `pessimistic_lock_wait_timeout`)
  - TLS (mTLS): `TIKV_CA_PATH`, `TIKV_CERT_PATH`, `TIKV_KEY_PATH`
  - Keyspace isolation: `TIKV_KEYSPACE` env var; per-tenant keyspace prefix `d_{db_id}_*`
  - All persistent data flows through `src/storage/tikv_store/` via `TikvStore`
  - Connection pooling: `src/pool.rs` (`TikvClientPool`, per-tenant, idle eviction after 300s)

**File Storage:**
- AWS S3-compatible object store (fs9 v2 backend)
  - SDK: `aws-sdk-s3` 1.110.0 + `aws-config` 1.8.13
  - Auth: AWS SDK credential chain (env vars, instance profile, etc.)
  - Config: per-tenant `ObjectStoreBinding` (bucket, region, endpoint, force_path_style)
  - Implementation: `src/extensions/fs/s3.rs`
  - Supports custom endpoint (`endpoint_url`) for S3-compatible stores (MinIO, etc.)
  - Supports pre-signed URLs for upload tokens

**Caching:**
- In-process only — no external cache
  - `TableStatsCache` — per-tenant table statistics (process-level)
  - `TriggerBodyCache` — trigger body cache (process-level)
  - `RlsPolicyCache` — RLS policy cache (process-level)
  - `DashMap`-based HNSW process-level LRU cache (`src/sql/hnsw/`)
  - Embedding result cache (per-tenant, in `src/extensions/context.rs`)
  - JWKS cache (process-level, 60s TTL, `src/auth/db9_auth.rs`)

## Authentication & Identity

**Auth Provider:**
- Custom password auth — bcrypt-hashed passwords stored in TiKV
  - Implementation: `src/auth/password.rs`, `src/auth/rbac.rs`
  - Bootstrap: `DB9_BOOTSTRAP_ADMIN_PASSWORD` + `DB9_BOOTSTRAP_ADMIN_USER` (default `admin`)

- JWT token auth — RS256 JWTs verified against JWKS or static public key
  - Mode: `DB9_AUTH_MODE=token` or `DB9_AUTH_MODE=both`
  - Implementation: `src/auth/db9_auth.rs`

- Connect key auth — `db9ck_*` prefixed keys, optionally introspected via external API
  - Implementation: `src/auth/db9_auth.rs` (`classify_db9_auth_material`)

- RBAC: role-based access control with PostgreSQL-compatible privilege model
  - Implementation: `src/auth/rbac.rs`
  - Superuser bootstrap via `DB9_BOOTSTRAP_ADMIN_*` or `DB9_DEV=1` + `DB9_DEV_ADMIN_PASSWORD`

## Monitoring & Observability

**Error Tracking:**
- None (no external error tracking service integrated)

**Logs:**
- `tracing` 0.1 + `tracing-subscriber` 0.3 with `EnvFilter`
- Log level controlled by `RUST_LOG` env var
- Custom observability metrics at `src/observability.rs`:
  - Query sampling (configurable rate via `DB9_OBS_SAMPLE_EVERY`, default 0.1%)
  - Slow query detection (configurable threshold via `DB9_OBS_SLOW_MS`, default 200ms)
  - In-memory sliding-window QPS and latency histograms (1-hour window, 60-second buckets)
  - Accessible via `pg_stat_statements`-style virtual catalog tables

**KV Stats:**
- Per-task KV read statistics tracking via task-local storage (`src/storage/kv_stats.rs`)

## CI/CD & Deployment

**Hosting:**
- Single binary deployment (`db9-server`)
- Docker container via `Dockerfile` (multi-arch, cross-compiles to `aarch64-unknown-linux-gnu`)
- `Dockerfile.dev` for development builds

**CI Pipeline:**
- Not detected in source. Scripts in `scripts/` for local test runs.
- `scripts/regression_gate.sh` — fast regression gate (< 5 min)
- `run_tests.sh` — full automated test suite (starts TiKV + db9-server)

**Test Infrastructure:**
- `scripts/integration_test.py` — Python integration test runner against live server
- `orm-tests/` — TypeScript ORM compatibility tests (TypeORM, Prisma, Sequelize, Drizzle, Knex, Kysely)
- `tests/` — 563 SQL test files

## Environment Configuration

**Required env vars:**
- `PD_ENDPOINTS` — TiKV PD cluster address(es)
- `DB9_BOOTSTRAP_ADMIN_PASSWORD` — initial admin password (first-run only)
- `PG_TLS_CERT` + `PG_TLS_KEY` — TLS cert/key (required for non-loopback without `DB9_INSECURE=1`)

**Optional critical env vars:**
- `DB9_AUTH_MODE` — auth strategy (`password` | `both` | `token`)
- `DB9_AUTH_JWKS_URL` — JWKS endpoint (required when `DB9_AUTH_MODE=token` or `both`)
- `EMBEDDING_API_KEY` — required for `db9_embed()` SQL function
- `TIKV_CA_PATH` / `TIKV_CERT_PATH` / `TIKV_KEY_PATH` — TiKV mTLS (required for TLS-secured TiKV)

**Secrets location:**
- All secrets via environment variables (no secrets file detected)
- No `.env` file committed

## Webhooks & Callbacks

**Incoming:**
- pgwire TCP connections on `PG_LISTEN_ADDR:PG_PORT` (default `127.0.0.1:5433`)
- fs9 WebSocket connections on `FS9_WS_LISTEN_ADDR:FS9_WS_PORT` (default port 8765, implementation at `src/extensions/fs/ws/`)

**Outgoing:**
- Embedding API calls from `db9_embed()` SQL function (configured endpoint)
- JWKS fetch for JWT verification (`DB9_AUTH_JWKS_URL`)
- Connect key introspection (`DB9_AUTH_CONNECT_KEY_INTROSPECT_URL`)
- `http_get()` / `http_post()` / etc. SQL extension functions (arbitrary URLs, user-initiated)
- S3 API calls for fs9 file operations

## ORM Compatibility

The following ORMs are tested for wire-protocol compatibility:
- **TypeORM** 0.3.17 — tests at `orm-tests/typeorm/`
- **Prisma** 5.7.0 — tests at `orm-tests/prisma/`
- **Sequelize** 6.35.0 — tests at `orm-tests/sequelize/`
- **Drizzle ORM** 0.29.0 — tests at `orm-tests/drizzle/`
- **Knex** 3.1.0 — tests at `orm-tests/knex/`
- **Kysely** 0.28.11 — tests at `orm-tests/kysely/`
- **node-postgres** (`pg`) 8.11.0 — direct driver tests at `orm-tests/pg-client/`

All ORMs connect via standard PostgreSQL wire protocol using the `pg` driver.

---

*Integration audit: 2026-03-17*

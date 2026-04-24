# ops-config — Runtime configuration registry

## Scope
- Authoritative registry of supported operator-facing runtime inputs.
- CLI flags, environment variables, defaults, and merge precedence.
- Security-sensitive operational posture (TLS, insecure escape hatches, bootstrap).

## Non-goals
- Repeating feature semantics that belong to other SoT modules.
- Documenting CI-only shell variables that are not read by `src/**`.
- Treating incidental internal inputs as supported operator knobs.

## External Contracts
- **[Stable] Merge order for server startup inputs**
  - Supported CLI flags override their corresponding environment variables.
  - When a CLI flag is absent, db9 falls back to the environment variable and then to the hardcoded default.
  - Evidence: `src/cli.rs`, `src/main.rs`.

- **[Stable] Security posture is secure-by-default**
  - Non-loopback pgwire and fs9 WebSocket listeners refuse cleartext startup unless TLS is configured or the operator explicitly opts into `DB9_INSECURE=1` or `DB9_DEV=1`.
  - First-superuser bootstrap requires `DB9_BOOTSTRAP_ADMIN_PASSWORD` unless `DB9_DEV=1`.
  - Evidence: `src/main.rs`, `src/auth/rbac.rs`.

## Entrypoints
- `src/cli.rs`
- `src/main.rs`
- `src/config.rs`
- `src/tls.rs`
- `src/pool.rs`
- `src/worker/config.rs`
- `src/cron/config.rs`
- `src/storage/backpressure.rs`
- `src/observability.rs`
- `src/extensions/http.rs`
- `src/extensions/fs/ws/protocol.rs`
- `src/protocol/handler/portal.rs`
- `src/sql/executor/table_utils/generate_series.rs`
- `src/storage/tikv_store/mod.rs`

## Configuration

### CLI Flags

| Flag | Env fallback | Default | Evidence | Notes |
|---|---|---|---|---|
| `--host` | `PG_LISTEN_ADDR` | `127.0.0.1` | `src/cli.rs`, `src/main.rs` | pgwire listen address. |
| `--port` | `PG_PORT` | `5433` | `src/cli.rs`, `src/main.rs` | pgwire listen port. |
| `--pd-endpoints` | `PD_ENDPOINTS` | `127.0.0.1:2379` | `src/cli.rs`, `src/main.rs` | Comma-separated PD endpoints. |
| `--keyspace` | `PG_KEYSPACE` | `default` | `src/cli.rs`, `src/main.rs` | Default tenant keyspace when username has no explicit override. |
| `--tls-cert` | `PG_TLS_CERT` | unset | `src/cli.rs`, `src/main.rs` | Requires matching `--tls-key` / `PG_TLS_KEY`. |
| `--tls-key` | `PG_TLS_KEY` | unset | `src/cli.rs`, `src/main.rs` | Requires matching `--tls-cert` / `PG_TLS_CERT`. |
| `--help` / `-h` | none | n/a | `src/cli.rs` | Prints usage and exits. |
| `--version` / `-V` | none | n/a | `src/cli.rs` | Prints build/version info and exits. |

### Core Server, TLS, and Bootstrap

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `PD_ENDPOINTS` | `127.0.0.1:2379` | `src/main.rs` | Comma-separated PD endpoints. |
| `PG_LISTEN_ADDR` | `127.0.0.1` | `src/main.rs` | Non-loopback cleartext startup is refused unless TLS or insecure escape hatch is enabled. |
| `PG_PORT` | `5433` | `src/main.rs` | pgwire listen port. |
| `PG_KEYSPACE` | `default` | `src/main.rs` | Default tenant keyspace when username has no keyspace prefix. |
| `PG_TLS_CERT` | unset | `src/main.rs`, `src/tls.rs` | Enables pgwire TLS only when paired with `PG_TLS_KEY`. |
| `PG_TLS_KEY` | unset | `src/main.rs`, `src/tls.rs` | Enables pgwire TLS only when paired with `PG_TLS_CERT`. |
| `PG_REQUIRE_TLS` | `false` | `src/main.rs` | Refuses non-TLS pgwire startup when enabled. |
| `DB9_AUTH_MODE` | `password` | `src/config.rs`, `src/protocol/handler/dynamic/startup.rs` | Authentication mode: `password` (legacy), `both` (password + token), `token` (token only). |
| `DB9_INSECURE` | `false` | `src/main.rs` | Explicitly permits insecure non-loopback startup/posture. |
| `DB9_DEV` | `false` | `src/main.rs`, `src/auth/rbac.rs` | Enables legacy dev bootstrap/insecure development behavior. |
| `DB9_BOOTSTRAP_ADMIN_USER` | `admin` | `src/auth/rbac.rs` | Initial superuser username when bootstrapping an empty keyspace. |
| `DB9_BOOTSTRAP_ADMIN_PASSWORD` | unset | `src/auth/rbac.rs` | Required for secure first-superuser bootstrap. |
| `DB9_TOKIO_STACK_MB` | `8` | `src/main.rs` | Tokio worker thread stack size in MiB. |
| `RUST_LOG` | `info` | `src/main.rs` | Standard tracing filter input consumed by `EnvFilter::try_from_default_env()`. |
| `TIKV_CA_PATH` | unset | `src/storage/tikv_store/mod.rs` | Enables TLS for PD/TiKV client when paired with cert/key. |
| `TIKV_CERT_PATH` | unset | `src/storage/tikv_store/mod.rs` | TiKV client certificate path. |
| `TIKV_KEY_PATH` | unset | `src/storage/tikv_store/mod.rs` | TiKV client key path. |

### Token Authentication (JWT / Connect-key)

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_AUTH_JWKS_URL` | unset | `src/auth/db9_auth.rs` | JWT verification via remote JWKS (preferred). |
| `DB9_AUTH_JWT_PUBLIC_KEY` | unset | `src/auth/db9_auth.rs` | JWT verification via RSA public key (PEM). |
| `DB9_AUTH_JWT_ALGORITHM` | `RS256` | `src/auth/db9_auth.rs` | Allowed JWT algorithms (comma-separated). Default: `RS256`. |
| `DB9_AUTH_ISSUER` | unset | `src/auth/db9_auth.rs` | Optional JWT issuer constraint (single value; used as fallback when `DB9_AUTH_ISSUERS` is unset). |
| `DB9_AUTH_ISSUERS` | unset | `src/auth/db9_auth.rs` | Optional JWT issuer constraint accepting multiple values (comma-separated, e.g. `https://auth9.example,https://legacy.example`). When set, overrides `DB9_AUTH_ISSUER`. |
| `DB9_AUTH_AUDIENCE` | `db9-server` | `src/auth/db9_auth.rs` | JWT audience constraint. |
| `DB9_AUTH_CONNECT_KEY_INTROSPECT_URL` | unset | `src/auth/db9_auth.rs` | Connect-key introspection endpoint URL. Request/response contract: see [docs/authentication.md §Connect-Key Introspection Contract](../authentication.md#connect-key-introspection-contract). |
| `DB9_AUTH_CONNECT_KEY_INTROSPECT_API_KEY` | unset | `src/auth/db9_auth.rs` | Optional `X-API-Key` header for connect-key introspection. |

### Server Defaults and Tenant Resource Limits

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_STATEMENT_TIMEOUT_MS` | `60000` | `src/config.rs` | Default statement timeout applied to new sessions. |
| `DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS` | `60000` | `src/config.rs` | Default idle-in-transaction timeout applied to new sessions. |
| `DB9_TCP_KEEPALIVE_IDLE_MS` | `60000` | `src/config.rs`, `src/main.rs` | TCP keepalive idle time for pgwire sockets; `0` disables keepalive. |
| `DB9_MAX_CONNECTIONS` | `1000` | `src/config.rs`, `src/main.rs` | Enforced with a connection semaphore; excess connections receive SQLSTATE `53300`. |
| `DB9_TENANT_QPS_LIMIT` | `0` (disabled) | `src/pool.rs` | Per-tenant QPS limiter. |
| `DB9_TENANT_MEMORY_QUOTA_BYTES` | `0` (unlimited) | `src/pool.rs` | Per-tenant aggregate statement memory quota. |

### Protocol and SQL Guardrails

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_MAX_SUSPENDED_PORTALS` | `32` | `src/protocol/handler/portal.rs` | Maximum suspended portals retained in memory. |
| `DB9_MAX_SUSPENDED_PORTAL_BUFFER_ROWS` | `10000` | `src/protocol/handler/portal.rs` | Row cap for buffered suspended-portal results. |
| `DB9_MAX_SUSPENDED_PORTAL_BUFFER_BYTES` | `16777216` | `src/protocol/handler/portal.rs` | Byte cap for buffered suspended-portal results. |
| `DB9_MAX_GENERATE_SERIES_ROWS` | `1000000` | `src/sql/executor/table_utils/generate_series.rs` | Guardrail for `generate_series`. |

### Worker and Cron

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_WORKER_ENABLED` | `true` | `src/worker/config.rs` | Master switch for background task execution on this node; SQL-serving processes still publish GC registry state. |
| `DB9_WORKER_POLL_MS` | `60000` | `src/worker/config.rs` | Minimum effective value is `100`. |
| `DB9_WORKER_MAX_CONCURRENT_JOBS` | `32` | `src/worker/config.rs` | Per-node worker concurrency cap. |
| `DB9_WORKER_ID` | `<hostname>:<pid>` | `src/worker/config.rs` | Overrides the auto-derived worker claim/logging ID. This is not the GC registry identity. |
| `DB9_WORKER_STATEMENT_TIMEOUT_MS` | `300000` | `src/worker/config.rs` | Whole-task timeout for non-cron worker SQL. `0` disables the timeout. |
| `DB9_CRON_JOB_TIMEOUT_MS` | `1800000` | `src/worker/config.rs` | Whole-job timeout for cron execution. `0` disables the timeout. |
| `DB9_WORKER_ORPHAN_TIMEOUT_SEC` | `300` | `src/worker/config.rs` | Claim GC orphan timeout. |
| `DB9_WORKER_GC_BATCH_SIZE` | `100` | `src/worker/config.rs` | Claim GC batch size. |
| `DB9_AUTO_ANALYZE_ENABLED` | `true` | `src/worker/config.rs` | Enables worker-driven auto-analyze. |
| `DB9_AUTO_ANALYZE_THRESHOLD` | `50` | `src/worker/config.rs` | Base threshold used by current auto-analyze policy. |
| `DB9_WORKER_GC_INTERVAL_SEC` | `600` | `src/worker/config.rs`, `src/worker/gc.rs` | Minimum effective value is `30`. |
| `DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC` | `600` | `src/worker/config.rs`, `src/worker/gc.rs` | Independent cadence for HNSW delta sweep/enqueue. Also runs S3 GC when `HNSW_S3_BUCKET` is set. |
| `DB9_WORKER_SYSTEM_KEYSPACE` | `_sys_worker` | `src/worker/config.rs` | Keyspace holding background task metadata. |
| `DB9_GC_SAFEPOINT_ENABLED` | `true` | `src/worker/config.rs`, `src/worker/gc.rs` | Enables PD safepoint advancement. |
| `DB9_GC_SAFEPOINT_INTERVAL_SEC` | `300` | `src/worker/config.rs`, `src/worker/gc.rs` | Minimum effective value is `30`; must remain below `DB9_GC_LIFE_TIME_SEC` on every SQL-serving node because GC registry heartbeats are unconditional. |
| `DB9_GC_LIFE_TIME_SEC` | `86400` | `src/worker/config.rs`, `src/worker/gc.rs` | MVCC retention window for time-based safepoint calculation. Active transactions are protected by direct registry tracking, not by this timeout; GC registry heartbeats older than this window are ignored and reaped. |
| `DB9_CRON_ENABLED` | `true` | `src/cron/config.rs` | Master switch for cron scheduling. |
| `DB9_CRON_POLL_MS` | `60000` | `src/cron/config.rs` | Values below default are clamped up to `60000`. |
| `DB9_CRON_MAX_RUNNING_JOBS` | `32` | `src/cron/config.rs` | Concurrent cron jobs per node. |
| `DB9_CRON_MAX_JOBS_PER_DB` | `50` | `src/cron/config.rs` | Per-database cron job cap. |
| `DB9_CRON_GC_INTERVAL_SEC` | `3600` | `src/cron/config.rs` | Cron run-history GC cadence. |
| `DB9_CRON_RUN_RETENTION_DAYS` | `7` | `src/cron/config.rs` | Retention for cron run history. |
| `DB9_CRON_ORPHAN_TIMEOUT_SEC` | `300` | `src/cron/config.rs` | Cron orphan timeout. |

### HNSW S3 Offload

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `HNSW_S3_BUCKET` | unset | `src/sql/hnsw/s3.rs` | S3 bucket for HNSW graph offload. When set, graph blobs are stored in S3 instead of TiKV. When unset, behavior is unchanged (TiKV-only with 8 MB frozen guard). |
| `HNSW_S3_REGION` | unset | `src/sql/hnsw/s3.rs` | S3 region. Falls back to `AWS_REGION` / `AWS_DEFAULT_REGION`. |
| `HNSW_S3_ENDPOINT` | unset | `src/sql/hnsw/s3.rs` | S3-compatible endpoint URL (e.g. MinIO). |
| `HNSW_S3_PREFIX` | `hnsw` | `src/sql/hnsw/s3.rs` | S3 key prefix for graph objects. |
| `HNSW_S3_FORCE_PATH_STYLE` | `false` | `src/sql/hnsw/s3.rs` | Use path-style URLs (required for MinIO). |
| `HNSW_CACHE_MAX_ENTRIES` | `64` | `src/sql/hnsw/s3.rs` | Max cached HNSW graph files (LRU). Increase for deployments with many hot vector indexes. |
| `HNSW_CACHE_DIR` | `/tmp/db9_hnsw_cache` | `src/sql/hnsw/s3.rs` | Base directory for cached graph files. db9 uses a `db9_hnsw_cache/` subdirectory under this path. Use a dedicated volume for high-QPS workloads. |

### Observability, Extensions, and fs9

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_OBS_ENABLED` | `true` | `src/observability.rs` | Enables in-process observability sampling/aggregation. |
| `DB9_OBS_SAMPLE_EVERY` | `1000` | `src/observability.rs` | Sample one in N statements. |
| `DB9_OBS_SLOW_MS` | `200` | `src/observability.rs` | Slow-statement threshold in milliseconds. |
| `DB9_OBS_MAX_SAMPLE_EVENTS` | `20000` | `src/observability.rs` | Maximum sampled events retained. |
| `DB9_OBS_MAX_SAMPLE_GROUPS` | `50` | `src/observability.rs` | Maximum distinct sampled query groups. |
| `DB9_OBS_MAX_SQL_LEN` | `512` | `src/observability.rs` | Maximum stored SQL text length for samples. |
| `DB9_HTTP_ALLOW_INSECURE` | `false` | `src/extensions/http.rs` | Allows `http://` outbound requests when enabled. |
| `EMBEDDING_API_KEY` | unset | `src/config.rs` | Required for embedding service availability. |
| `EMBEDDING_ENDPOINT` | DashScope-compatible v1 embeddings URL | `src/config.rs` | Normalized to end in `/embeddings`. |
| `EMBEDDING_BASE_URL` | unset | `src/config.rs` | Fallback alias when `EMBEDDING_ENDPOINT` is unset. |
| `EMBEDDING_MODEL` | `text-embedding-v4` | `src/config.rs` | Non-v4 values are forced back to `text-embedding-v4`. |
| `EMBEDDING_DIMENSIONS` | `1024` | `src/config.rs` | Default embedding dimensions. |
| `FS9_WS_PORT` | `5480` | `src/main.rs`, `src/extensions/fs/ws/protocol.rs` | `0` disables the fs9 WebSocket server. |
| `FS9_WS_LISTEN_ADDR` | `127.0.0.1` | `src/main.rs`, `src/extensions/fs/ws/protocol.rs` | Non-loopback cleartext bind is refused unless TLS or insecure escape hatch is enabled. |

### TiKV Backpressure

| Key | Default | Evidence | Notes |
|---|---|---|---|
| `DB9_TIKV_BP_ENABLED` | `false` | `src/storage/backpressure.rs` | Enables adaptive TiKV admission control. |
| `DB9_TIKV_BP_MIN_PERMITS` | `4` | `src/storage/backpressure.rs` | Lower bound for adaptive permit count. |
| `DB9_TIKV_BP_MAX_PERMITS` | `256` | `src/storage/backpressure.rs` | Upper bound for adaptive permit count. |
| `DB9_TIKV_BP_LATENCY_THRESHOLD_MS` | `200` | `src/storage/backpressure.rs` | P99 latency threshold driving AIMD decrease. |
| `DB9_TIKV_BP_WINDOW_SIZE` | `1024` | `src/storage/backpressure.rs` | Rolling latency window size. |
| `DB9_TIKV_BP_EVAL_INTERVAL` | `128` | `src/storage/backpressure.rs` | Number of completions between evaluations. |

### Unsupported / Incidental Inputs
- `HOSTNAME` is currently used only as a best-effort ingredient when auto-deriving the default worker ID. It is not treated as a supported configuration contract.

### Notes
- `DB9_WORKER_ENABLED=false` disables background task execution on that node, but SQL-serving processes still initialize the GC registry and publish transaction liveness for safepoint protection.

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/lint`, `ci:.github/workflows/ci.yml/integration-tests`, `ci:.github/workflows/doc-lint.yml/doc-lint`
- Local reproduce (typical):
  - `cargo fmt -- --check && cargo clippy --workspace --all-targets -- -D warnings`
  - `./run_tests.sh`
  - `uv run scripts/doc_lint.py`

## Change Management
- Any PR that adds, removes, renames, or changes the default/meaning of a supported operator-facing input MUST update this document and keep `docs/sot/modules.yaml` and `docs/sot/README.md` aligned.
- New config keys MUST be documented here exactly once; other SoT docs MUST cross-link instead of restating them.
- Gate changes require corresponding updates in `./testing-gates.md`.
- Reference: https://github.com/c4pt0r/db9/issues/368

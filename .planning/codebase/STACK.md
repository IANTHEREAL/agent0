# Technology Stack

**Analysis Date:** 2026-03-17

## Languages

**Primary:**
- Rust (edition 2021, MSRV 1.88) — all server-side code in `src/`

**Secondary:**
- TypeScript 5.3 — ORM compatibility tests in `orm-tests/`
- Python 3 — integration test runner (`scripts/integration_test.py`)
- SQL — test files in `tests/` (563 files)

## Runtime

**Environment:**
- Rust tokio async runtime (multi-thread), configurable stack via `DB9_TOKIO_STACK_MB` (default 8 MB)
- Node.js ≥18.0.0 for ORM tests

**Recursion limit:** `#![recursion_limit = "256"]` (deep async call chains in query handler)

**Package Manager:**
- Rust: `cargo` with `resolver = "3"`, lockfile `Cargo.lock` present
- Node.js: `npm`, lockfile `package-lock.json` present

## Frameworks

**Core:**
- `tokio` 1.36 (full features) — async runtime
- `pgwire` 0.28 (patched at `crates/pgwire`) — PostgreSQL wire protocol server
- `sqlparser` 0.40 (with visitor feature) — SQL parsing

**Protocol:**
- `tokio-rustls` 0.26 + `rustls-pemfile` 2.0 + `rustls-pki-types` 1.0 — TLS termination at pgwire layer
- `tokio-tungstenite` 0.24 — WebSocket transport for fs9 SDK

**Testing (Rust):**
- Rust built-in `#[test]` and `#[tokio::test]` — unit tests co-located in source files
- Python `scripts/integration_test.py` — SQL integration tests against live server

**Testing (TypeScript):**
- `vitest` 1.0 — ORM compatibility test runner
- Config at `orm-tests/vitest.config.ts`

**Build/Dev:**
- `cargo build` / `cargo build --release`
- Cross-compilation to `aarch64-unknown-linux-gnu` via Docker (`Dockerfile`)
- `build.rs` — build-time metadata (BUILD_GIT_HASH, BUILD_DATE)
- `clippy.toml` — clippy config

## Key Dependencies

**Critical:**
- `tikv-client` (vendored at `vendor/tikv-client`) — TiKV KV store client; vendored to add `pessimistic_lock_wait_timeout` support for SKIP LOCKED / NOWAIT semantics
- `pgwire` 0.28 (patched at `crates/pgwire`) — pgwire protocol; patched for db9-specific behavior
- `sqlparser` 0.40 — SQL AST; output feeds the Analyzer pipeline

**Storage / Serialization:**
- `serde` 1.0 + `serde_json` 1.0 — data serialization
- `bincode` 1.3 — binary serialization in storage layer (technical debt, see issue #694)
- `rmp-serde` 1 — MessagePack serialization
- `memcomparable` 0.2 — byte-comparable key encoding for TiKV range scans

**Data Types:**
- `rust_decimal` 1.33 (serde, db-postgres, maths features) — NUMERIC/DECIMAL type
- `bigdecimal` 0.4 — decimal arithmetic
- `chrono` 0.4 + `chrono-tz` 0.10 — date/time types and timezone support
- `uuid` 1.0 (v4) — UUID generation
- `bytes` 1.5 — byte buffer management

**Crypto / Auth:**
- `sha2` 0.10 — SHA-256 hashing
- `hmac` 0.12 — HMAC computation
- `jsonwebtoken` 9 — JWT decode/verify (RS256, per-tenant JWKS)
- `md5` 0.7 — MD5 for PostgreSQL password auth (MD5 challenge-response)
- `base64` 0.22 — base64 encoding/decoding

**NLP / Search:**
- `jieba-rs` 0.7 — Chinese tokenizer for full-text search GIN indexes
- `rust-stemmers` 1.2 — Snowball stemmer for full-text search
- `rust_icu_ucol` 5.0 + `rust_icu_ustring` 5.0 — ICU collation support (requires `libicu72` at runtime)
- `regex` 1.10 — regular expression evaluation

**Vector Search:**
- `usearch` 0.21 — HNSW approximate nearest neighbor index (FFI to usearch C++ library)

**HTTP / Networking:**
- `reqwest` 0.12 (json feature) — outbound HTTP for embeddings, JWKS fetch, http extension functions
- `globset` 0.4 — glob pattern matching for fs9 file system

**Cloud / Storage:**
- `aws-config` 1.8.13 + `aws-sdk-s3` 1.110.0 — S3-compatible object store for fs9 v2 backend
- `parquet` 57 + `arrow` 57 (optional, default enabled) — Parquet file reading (Arrow columnar format)

**Scheduling:**
- `cron` 0.15 — cron expression parsing for pg_cron-compatible scheduler

**Utility:**
- `dashmap` 6.0 — concurrent hash map (tenant semaphores, caches)
- `once_cell` 1.19 — lazy/once initialization
- `stacker` 0.1 — stack overflow guard (deep SQL recursion)
- `itoa` 1.0 + `ryu` 1.0 — fast integer/float to string conversion
- `csv` 1.3 — CSV parsing for COPY CSV
- `hex` 0.4 — hex encoding
- `rand` 0.8 — random number generation

## Configuration

**Environment Variables (required for production):**
- `PD_ENDPOINTS` — TiKV PD endpoint(s), default `127.0.0.1:2379`
- `PG_TLS_CERT` / `PG_TLS_KEY` — TLS certificate/key paths (required for non-loopback, unless `DB9_INSECURE=1`)
- `DB9_BOOTSTRAP_ADMIN_PASSWORD` — initial superuser password (set on first start)
- `DB9_AUTH_MODE` — `password` | `both` | `token` (default `password`)

**Environment Variables (optional):**
- `PG_PORT` — pgwire listen port (default `5433`)
- `PG_LISTEN_ADDR` — bind address (default `127.0.0.1`)
- `PG_REQUIRE_TLS` — enforce TLS for all connections
- `DB9_DEV` — development mode (insecure behaviors, requires `DB9_DEV_ADMIN_PASSWORD`)
- `DB9_INSECURE` — explicitly allow non-TLS on non-loopback
- `DB9_TOKIO_STACK_MB` — tokio thread stack size in MB (default `8`)
- `DB9_MAX_CONNECTIONS` — connection limit (default `1000`)
- `DB9_STATEMENT_TIMEOUT_MS` — statement timeout (default `60000`)
- `DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS` — idle-in-txn timeout (default `60000`)
- `DB9_TENANT_QPS_LIMIT` — per-tenant QPS limit (`0` = disabled)
- `DB9_TENANT_MEMORY_QUOTA_BYTES` — per-tenant memory quota (`0` = unlimited)
- `DB9_AUTH_JWKS_URL` — JWKS endpoint URL for JWT auth
- `DB9_AUTH_JWT_PUBLIC_KEY` — PEM public key for static JWT verification
- `DB9_AUTH_ISSUER` / `DB9_AUTH_AUDIENCE` / `DB9_AUTH_JWT_ALGORITHM` — JWT validation params
- `DB9_AUTH_CONNECT_KEY_INTROSPECT_URL` / `DB9_AUTH_CONNECT_KEY_INTROSPECT_API_KEY` — connect key introspection
- `TIKV_CA_PATH` / `TIKV_CERT_PATH` / `TIKV_KEY_PATH` / `TIKV_KEYSPACE` — TiKV mTLS + keyspace config
- `EMBEDDING_API_KEY` / `EMBEDDING_ENDPOINT` / `EMBEDDING_MODEL` / `EMBEDDING_DIMENSIONS` — vector embedding config
- `DB9_HTTP_ALLOW_INSECURE` — allow non-HTTPS in `http_get`/`http_post` SQL functions
- `DB9_OBS_ENABLED` / `DB9_OBS_SLOW_MS` / `DB9_OBS_SAMPLE_EVERY` — observability config
- `FS9_WS_PORT` / `FS9_WS_LISTEN_ADDR` — fs9 WebSocket server config
- `DB9_CRON_ENABLED` / `DB9_CRON_POLL_MS` / `DB9_CRON_MAX_RUNNING_JOBS` — cron scheduler config
- `DB9_WORKER_ENABLED` / `DB9_WORKER_POLL_MS` / `DB9_WORKER_MAX_CONCURRENT_JOBS` — background worker config
- `DB9_DEFAULT_TEXT_SEARCH_CONFIG` — default FTS configuration (default `simple`)
- `RUST_LOG` / logging controlled by `tracing-subscriber` with `EnvFilter`

**Build:**
- `Cargo.toml` — Rust dependencies and features
- `clippy.toml` — Clippy lint configuration
- `build.rs` — injects build metadata (git hash, build date)
- `Dockerfile` / `Dockerfile.dev` — container build (cross-compile to `aarch64-unknown-linux-gnu`)

## Platform Requirements

**Development:**
- Rust 1.88+
- TiKV with PD running (default `127.0.0.1:2379`)
- libicu (ICU collation support) at link/runtime
- protobuf-compiler (for TiKV client build)
- Node.js ≥18 (ORM tests only)

**Production:**
- Debian Bookworm (slim) runtime image
- `libssl3`, `ca-certificates`, `libicu72` runtime libraries
- TiKV cluster (PD + TiKV nodes)
- Deployed binary: `db9-server` (single binary, exposes pgwire on port 5433 by default)

---

*Stack analysis: 2026-03-17*

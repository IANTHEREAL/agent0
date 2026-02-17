# ops-config — Configuration keys & operational defaults

## Scope
- Runtime configuration keys and defaults (env vars; no CLI flags are currently implemented).
- TLS configuration and security defaults.
- Runtime limits/backpressure knobs and their operational meaning.
- Config key uniqueness: config keys MUST be defined exactly once across SoT docs.

## Non-goals
- Feature semantics (authoritative: other module SoT docs).
- Duplicating config definitions across multiple docs (use cross-links instead).
- Documenting CI/test-only env vars that are not read by `src/**`.

## Entrypoints
- `src/main.rs` (server bootstrap; TiKV endpoints; port; keyspace; TLS env wiring)
- `src/tls.rs` (TLS acceptor setup)
- `src/observability.rs` (observability knobs)
- `src/extensions/http.rs` (HTTP extension security knob)
- `src/sql/trigger_worker.rs` (async trigger worker knobs)
- `src/protocol/handler/portal.rs` (protocol resource limits)
- `docs/configuration.md` (non-SoT doc; may drift — this file is the SoT for keys)

## Configuration

| Key | Type | Default | Evidence (read site) | Notes |
|---|---|---|---|---|
| `PD_ENDPOINTS` | env | `127.0.0.1:2379` | `src/main.rs` (`async_main`) | Comma-separated PD endpoints. |
| `PG_PORT` | env | `5433` | `src/main.rs` (`async_main`) | Listening port; parse failures fall back to default. |
| `PG_LISTEN_ADDR` | env | `127.0.0.1` | `src/main.rs` (`async_main`) | Listening address; set to `0.0.0.0` to accept non-loopback connections. When set to non-loopback and TLS is disabled, startup fails unless `PGTIKV_INSECURE=1` or `PGTIKV_DEV=1`. |
| `PG_KEYSPACE` | env | `default` | `src/main.rs` (`async_main`); `src/sql/trigger_worker.rs` (`bootstrap_active_keyspaces`) | Default tenant keyspace when client username has no explicit keyspace; also used as trigger worker fallback active keyspace. |
| `PG_TLS_CERT` | env | unset (TLS disabled) | `src/main.rs` (`async_main`) | TLS is enabled only when both `PG_TLS_CERT` and `PG_TLS_KEY` are set and `tls::setup_tls` succeeds. |
| `PG_TLS_KEY` | env | unset (TLS disabled) | `src/main.rs` (`async_main`) | See `PG_TLS_CERT`. |
| `PG_REQUIRE_TLS` | env | `false` | `src/main.rs` (`async_main`) | When enabled, server requires TLS for all pgwire connections; startup fails if TLS is not configured. |
| `PGTIKV_INSECURE` | env | `false` | `src/main.rs` (`async_main`); `src/protocol/handler/dynamic.rs` (`on_startup`) | Explicit escape hatch: allows starting without TLS on non-loopback binds and allows non-TLS cleartext auth for non-loopback clients (unsafe; DO NOT use in production). |
| `PGTIKV_DEV` | env | `false` | `src/main.rs` (`async_main`); `src/auth/rbac.rs` (`AuthManager::bootstrap`) | Dev-only escape hatch: allows legacy insecure bootstrap (default superuser) and relaxes non-TLS auth restrictions (unsafe; DO NOT use in production). |
| `PGTIKV_BOOTSTRAP_ADMIN_USER` | env | `admin` | `src/auth/rbac.rs` (`AuthManager::bootstrap`) | Initial superuser username for bootstrapping when no superuser exists yet. |
| `PGTIKV_BOOTSTRAP_ADMIN_PASSWORD` | env | unset | `src/auth/rbac.rs` (`AuthManager::bootstrap`) | Required to bootstrap the first superuser when no superuser exists yet (non-dev mode). MUST NOT be logged. |
| `PGTIKV_TOKIO_STACK_MB` | env | `4` | `src/main.rs` (`main`) | Per-runtime worker thread stack size (MiB); must parse as `usize` and be `> 0`. |
| `PGTIKV_OBS_ENABLED` | env | `true` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Boolean parsing is best-effort; invalid values keep the default. |
| `PGTIKV_OBS_SAMPLE_EVERY` | env | `1000` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Sample 1 in N statements; must parse as `u64` and be `> 0`. |
| `PGTIKV_OBS_SLOW_MS` | env | `200` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Slow query threshold in ms; stored internally as µs. |
| `PGTIKV_OBS_MAX_SAMPLE_EVENTS` | env | `20000` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Cap for sampled events kept in-memory; must be `> 0`. |
| `PGTIKV_OBS_MAX_SAMPLE_GROUPS` | env | `50` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Cap for distinct query sample groups; must be `> 0`. |
| `PGTIKV_OBS_MAX_SQL_LEN` | env | `512` | `src/observability.rs` (`ObservabilityConfig::from_env`) | Max SQL length stored for sampled queries; must be `> 0`. |
| `PGTIKV_HTTP_ALLOW_INSECURE` | env | `false` | `src/extensions/http.rs` (`allow_insecure_http`) | When true, allows non-HTTPS HTTP extension requests; accepts `"1"` or case-insensitive `"true"`. |
| `PGTIKV_MAX_GENERATE_SERIES_ROWS` | env | `1000000` | `src/sql/executor/table_utils.rs` (`max_generate_series_rows`) | Guardrail for `generate_series`; must parse as `usize` and be `> 0`. |
| `PGTIKV_MAX_SUSPENDED_PORTALS` | env | `32` | `src/protocol/handler/portal.rs` (`max_suspended_portals`) | Upper bound for suspended portals kept in memory; must parse as `usize` and be `> 0`. |
| `PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_ROWS` | env | `10000` | `src/protocol/handler/portal.rs` (`max_suspended_portal_buffer_rows`) | Row count cap for buffered suspended-portal rows; must parse as `usize` and be `> 0`. |
| `PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_BYTES` | env | `16777216` | `src/protocol/handler/portal.rs` (`max_suspended_portal_buffer_bytes`) | Byte cap for buffered suspended-portal rows; must parse as `usize` and be `> 0`. |
| `PGTIKV_TRIGGER_ENABLED` | env | `true` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Boolean parsing accepts `1/0`, `true/false`, `yes/no`, `on/off` (case-insensitive). |
| `PGTIKV_TRIGGER_POLL_MS` | env | `100` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Poll interval for background worker; must parse as `u64` and be `> 0`. |
| `PGTIKV_TRIGGER_GC_INTERVAL_SEC` | env | `60` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | GC interval; must parse as `u64` and be `> 0`. |
| `PGTIKV_TRIGGER_DONE_RETENTION_SEC` | env | `3600` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | DONE retention; must parse as `u64` and be `> 0`. |
| `PGTIKV_TRIGGER_DLQ_RETENTION_DAYS` | env | `7` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | DLQ retention; must parse as `u64` and be `> 0`. |
| `PGTIKV_TRIGGER_ORPHAN_TIMEOUT_SEC` | env | `300` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Orphan timeout; must parse as `u64` and be `> 0`. |
| `PGTIKV_TRIGGER_QUEUE_LIMIT` | env | `10000` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Default max in-memory queue depth per keyspace; must parse as `usize` and be `> 0`. |
| `PGTIKV_TRIGGER_BATCH_SIZE` | env | `10` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Default max events per batch; must parse as `usize` and be `> 0`. |
| `PGTIKV_TRIGGER_MAX_RETRIES` | env | `3` | `src/sql/trigger_worker.rs` (`TriggerWorkerConfig::from_env`) | Default max retries per event; must parse as `u8` and be `> 0`. |
| `PGTIKV_TRIGGER_NODE_ID` | env | auto-derived | `src/sql/trigger_queue.rs` (`node_id`) | When set, must parse as `u16` and be in `[0, 1023]`; for multi-node deployments, set a unique value per node to avoid ID collisions. |
| `HOSTNAME` | env | unset | `src/sql/trigger_queue.rs` (`node_id`) | Best-effort input used to derive `PGTIKV_TRIGGER_NODE_ID` when not explicitly set. |

## Verification (Gates)
- `ci:.github/workflows/orm-tests.yml/lint` (required): `cargo fmt -- --check && cargo clippy`
- `ci:.github/workflows/orm-tests.yml/test` (required): `./run_tests.sh` (boots a local non-TLS instance with explicit bootstrap + insecure flags; does not assert TLS handshake)

## Change Management
- Any PR that adds/removes/renames a config key, changes a default, or changes security posture (TLS / HTTP insecure) MUST update this document and keep `docs/sot/modules.yaml` + `docs/sot/README.md` consistent.
- Any change that adds/removes/relaxes/tightens a gate MUST have a DR/ADR per #368 rules and update `docs/sot/testing-gates.md`.
- Track cross-module overlaps via `xref` in `docs/sot/modules.yaml`; other SoT docs MUST link here instead of restating config keys.
- Reference: https://github.com/c4pt0r/tipg/issues/368

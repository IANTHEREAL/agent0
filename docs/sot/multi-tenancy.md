# multi-tenancy — Tenant routing and isolation (current behavior)

## Scope
- Tenant username parsing and connection-time keyspace routing.
- Cross-tenant isolation for persistent data and tenant-scoped in-memory state.
- Per-tenant pooling, caches, and resource accounting.

## Non-goals
- Storage key encoding details (authoritative: `./storage-format.md`).
- RBAC policy semantics (authoritative: `./auth-rbac.md`).
- pgwire message framing (authoritative: `./protocol-pgwire.md`).

## External Contracts
- **[Stable] Connection routing via username**
  - Tenant routing accepts `<keyspace>.<username>` and `<keyspace>:<username>`.
  - Usernames without a valid separator route to the default keyspace from `PG_KEYSPACE` (or `default` if unset).
  - Evidence: `src/protocol/handler/tenant.rs`, `src/protocol/handler/dynamic/startup.rs`.

- **[Stable] Keyspace binding is immutable for a connection**
  - The effective keyspace is chosen during startup/authentication and MUST NOT change for the lifetime of that connection.
  - Evidence: `src/protocol/handler/dynamic/startup.rs`, `src/pool.rs`.

- **[Stable] No cross-keyspace persistent access**
  - Persistent data access is isolated by TiKV keyspace; a connection bound to keyspace A MUST NOT read or write keyspace B.
  - Evidence: `src/pool.rs`, `src/storage/tikv_store/mod.rs`.

- **[Stable] Tenant-scoped in-memory resources are isolated**
  - Tenant pooling isolates at least:
    - `TriggerBodyCache`,
    - `TableStatsCache`,
    - tenant memory accounting,
    - per-tenant connection/QPS bookkeeping.
  - Evicting or reaping one tenant entry MUST NOT affect another tenant's caches/accounting.
  - Evidence: `src/pool.rs`, `src/protocol/handler/dynamic/startup.rs`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/protocol/handler/tenant.rs`
- `src/protocol/handler/dynamic/startup.rs`
- `src/pool.rs`
- `src/storage/tikv_store/mod.rs`
- `src/session_context.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/regression-gate`, `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`
  - `cargo test`

## Change Management
- Any change to tenant routing, keyspace isolation, or tenant-scoped caches/accounting MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368

# Multi-Tenancy Contracts

## Scope

- Tenant username parsing and keyspace routing.
- Keyspace isolation invariants for persistent data.
- Per-tenant resource isolation (stats cache, schema cache).

## Non-goals

- Storage key encoding details (authoritative: [storage-format](./storage-format.md)).
- Auth policy and privilege model (authoritative: [auth-rbac](./auth-rbac.md)).
- pgwire startup message framing (authoritative: [protocol-pgwire](./protocol-pgwire.md)).

## Contracts (MUST)

- **Username format**: Tenant routing MUST support two separator formats:
  - `<keyspace>.<username>` (dot separator, takes precedence)
  - `<keyspace>:<username>` (colon separator)
  - Usernames without a separator MUST route to the default keyspace.
- **No cross-keyspace access**: A connection bound to keyspace A MUST NOT be able to read or write data in keyspace B. This is enforced at the TiKV client pool level.
- **All persistent data scoped to tenant**: Every persistent key (tables, indexes, schemas, sequences, auth, statistics, worker tasks) MUST be scoped to the connection's keyspace.
- **Per-tenant cache isolation**: `TableStatsCache` and schema cache MUST be isolated per tenant. Evicting one tenant's cache MUST NOT affect another's.
- **Default keyspace**: When no separator is present in the username, the connection MUST route to the keyspace specified by `PG_KEYSPACE` (default: `"default"`).
- **Keyspace immutability**: The keyspace for a connection MUST be determined at connection time and MUST NOT change for the lifetime of that connection.

## Configuration

This module MUST NOT redefine config keys. Relevant keys are defined exactly once in [ops-config](./ops-config.md):
- `PG_KEYSPACE` — Default keyspace name (default: `"default"`)
- `DB9_BOOTSTRAP_ADMIN_PASSWORD`, `DB9_BOOTSTRAP_ADMIN_USER` — Per-keyspace bootstrap

## Entrypoints

- `src/protocol/handler/tenant.rs` — `parse_tenant_username()`: username parsing and keyspace extraction
- `src/pool.rs` — `TikvClientPool`: keyspace-isolated TiKV client acquisition
- `src/session_context.rs` — Tokio task-local session context (timezone, search path isolation)

## Verification (Gates)

- `ci:.github/workflows/regression-gate.yml/regression-gate`
- `ci:.github/workflows/orm-tests.yml/test`
- `cmd:cargo test`

## Change Management

Any change to tenant routing, keyspace isolation, or per-tenant resource isolation MUST update this document and the corresponding module entry in `docs/sot/modules.yaml`. Breaking changes require DR/ADR per #368 rules.

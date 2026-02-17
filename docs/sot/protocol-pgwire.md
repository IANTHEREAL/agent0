# protocol-pgwire — PostgreSQL wire protocol surface (pgwire)

## Scope
- pgwire startup + authentication handshake (wire-level messages).
- Simple query protocol message handling.
- Extended query protocol framing (parse/bind/describe/execute) and portal suspension behavior.
- Tenant username parsing (`tenant.user` / `tenant:user`) and startup parameters that affect session settings.
- COPY protocol surface handled at the pgwire layer (`COPY ... FROM STDIN`, `COPY ... TO STDOUT`).

## Non-goals
- RBAC policy semantics and privilege model (authoritative: `./auth-rbac.md`).
- SQL parsing/planning/execution semantics (authoritative: `./sql-engine.md`).
- Persistent storage format / keyspace isolation invariants (authoritative: `./storage-format.md`).

## External Contracts
- **[Stable] Authentication mechanism**
  - The server MUST use cleartext password authentication on startup (`Authentication::CleartextPassword`) when accepting a connection.
  - When the connection is non-TLS:
    - if `PG_REQUIRE_TLS=1`, the server MUST reject the connection deterministically;
    - otherwise, non-loopback clients are rejected unless `PGTIKV_INSECURE=1` or `PGTIKV_DEV=1`.
  - Evidence: `src/protocol/handler/dynamic.rs` (`impl StartupHandler for DynamicPgHandler`, `on_startup`).

- **[Stable] Tenant keyspace routing via username**
  - The server MUST accept usernames in the form `tenant.user` or `tenant:user` to override the effective keyspace for the connection.
  - Parsing MUST split on the **first** separator occurrence. `.` MUST take precedence over `:` when both are present.
  - If the separator is missing or produces an empty tenant/user part, the server MUST treat the username as having **no** keyspace override.
  - Evidence: `src/protocol/handler/tenant.rs` (`parse_tenant_username`), `src/protocol/handler/tests.rs` (`test_parse_tenant_username_*`).
  - Cross-link: keyspace isolation invariants are specified in `./storage-format.md`.

- **[Stable] Startup parameters → session settings**
  - If startup parameter `options` is present, the server MUST parse `-c key=value` pairs and attempt to apply them as session settings.
  - Setting keys MUST be lowercased before applying; unknown settings SHOULD be ignored (current behavior: warn + continue).
  - If startup parameter `application_name` is present, the server MUST attempt to apply it to the session setting `application_name`.
  - Evidence: `src/protocol/handler/mod.rs` (`parse_startup_options`), `src/protocol/handler/dynamic.rs` (`on_startup` applying `session.set_known_setting`).

- **[Stable] Extended query: portal suspension buffering limits**
  - When executing an extended-protocol portal with a row limit (`max_rows > 0`), if the result set exceeds `max_rows`, the server MUST:
    - send `PortalSuspended`, and
    - buffer remaining rows in-memory for subsequent `Execute` calls on the same portal.
  - The server MUST enforce buffer limits; if exceeded, it MUST error with SQLSTATE `54000` and mention the override env vars in the message.
  - Evidence: `src/protocol/handler/portal.rs` (`send_limited_query_response`, `max_suspended_*` helpers).

- **[Experimental] COPY surface (simple query only; syntax-limited)**
  - `COPY ... FROM STDIN` and `COPY ... TO STDOUT` are recognized only when `COPY` begins at statement start after stripping leading whitespace/comments.
  - Identifiers in the recognized COPY patterns are restricted to `\\w+` (unquoted); quoted identifiers and complex COPY options are not covered by the current parser.
  - `COPY FROM STDIN` MUST enforce a max line size guardrail (currently `32 MiB`).
  - Evidence: `src/protocol/handler/dynamic.rs` (`parse_copy_command`, `parse_copy_to_command`), `src/protocol/handler/copy/mod.rs` (`MAX_COPY_FROM_STDIN_LINE_BYTES`), `src/protocol/handler/tests.rs` (`test_parse_copy_*`).
  - Cross-link: the actual SQL semantics of `COPY` statements (if/when supported beyond this pgwire shortcut) belong to `./sql-engine.md`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`:
- `PG_PORT`
- `PG_TLS_CERT`, `PG_TLS_KEY`
- `PG_REQUIRE_TLS`
- `PG_KEYSPACE`
- `PGTIKV_INSECURE`
- `PGTIKV_DEV`
- `PGTIKV_MAX_SUSPENDED_PORTALS`
- `PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_ROWS`
- `PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_BYTES`

## Entrypoints
- `src/main.rs` (`main`)
- `src/protocol/handler/dynamic.rs` (`DynamicPgHandler`, `authenticate_user`)
- `src/protocol/handler/tenant.rs` (`parse_tenant_username`)
- `src/protocol/handler/portal.rs` (portal suspension buffering)
- `src/protocol/copy_format.rs`
- `crates/pgwire/src/api/auth/mod.rs` (`StartupHandler`)
- `crates/pgwire/src/api/auth/cleartext.rs`
- `crates/pgwire/src/tokio/server.rs`
- `crates/pgwire/src/messages/codec.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/regression-gate.yml/regression-gate`, `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`

## Change Management
- Any change to pgwire handshake semantics, username/keyspace parsing, extended query buffering limits, or COPY recognition MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes to protocol surface (including error code changes) require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/tipg/issues/368

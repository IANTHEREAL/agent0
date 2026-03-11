# protocol-pgwire — PostgreSQL wire protocol surface (pgwire)

## Scope
- pgwire startup/authentication handshake.
- Simple-query and extended-query protocol handling.
- Tenant username parsing and startup parameters that map into session settings.
- COPY protocol handling at the pgwire layer.

## Non-goals
- RBAC policy semantics (authoritative: `./auth-rbac.md`).
- SQL planning/execution semantics once a statement enters the SQL engine (authoritative: `./sql-engine.md`).
- Storage/keyspace isolation invariants (authoritative: `./storage-format.md` and `./multi-tenancy.md`).

## External Contracts
- **[Stable] Authentication mechanism**
  - db9 uses cleartext-password startup auth (`PasswordMessage`) as the transport for authentication material.
  - Authentication mode is controlled by `DB9_AUTH_MODE` (`password|both|token`).
  - When token auth is enabled (`both|token`), db9 interprets `PasswordMessage` as DB9 auth material (JWT connect-token or `db9ck_` connect-key) and requires TLS unless `DB9_DEV=1` or `DB9_INSECURE=1`.
  - Non-TLS connections are rejected when `PG_REQUIRE_TLS=1`; otherwise non-loopback cleartext auth is rejected unless `DB9_INSECURE=1` or `DB9_DEV=1`.
  - Evidence: `src/protocol/handler/dynamic/startup.rs`, `src/config.rs`, `src/auth/db9_auth.rs`, `src/main.rs`.

- **[Stable] Tenant keyspace routing via username**
  - Usernames of the form `tenant.user` or `tenant:user` override the effective keyspace.
  - Parsing splits on the first separator; `.` takes precedence over `:`.
  - Invalid or empty splits fall back to the default keyspace.
  - Evidence: `src/protocol/handler/tenant.rs`, `src/protocol/handler/tests.rs`.

- **[Stable] Startup parameters map into session settings**
  - `options` is parsed as `-c key=value` pairs and applied as session settings where recognized.
  - `application_name` is mapped into the session setting of the same name.
  - Unknown settings are ignored with warning, not fatal.
  - Evidence: `src/protocol/handler/mod.rs`, `src/protocol/handler/dynamic/startup.rs`.

- **[Stable] Extended Parse has one parse authority**
  - `Db9QueryParser::parse_sql()` is the single parse authority for extended-protocol Parse.
  - Parseable SQL stores its AST for later use; empty SQL and fallback-accepted utility SQL do not retain cached AST.
  - Evidence: `src/protocol/handler/query_parser.rs`.

- **[Experimental] Parser-boundary utility fallback is explicit**
  - When `sqlparser` rejects a statement but the raw-SQL compatibility filter accepts it as a supported utility shape, the extended protocol returns `RawSqlUtility` instead of failing Parse.
  - This exception is limited to the documented utility surface; it is not a hidden fallback inside analyzed query execution.
  - Evidence: `src/protocol/handler/query_parser.rs`, `src/sql/raw_sql.rs`.

- **[Stable] Portal suspension uses bounded in-memory buffering**
  - Extended-protocol `Execute` with `max_rows > 0` uses suspended portals and buffers remaining rows in memory for subsequent executes on the same portal.
  - Buffer limits are enforced; exceeding them errors with SQLSTATE `54000`.
  - Evidence: `src/protocol/handler/portal.rs`.

- **[Experimental] COPY surface remains protocol-special-cased**
  - `COPY ... FROM STDIN` and `COPY ... TO STDOUT` are recognized by the pgwire layer when COPY begins at statement start after stripping leading whitespace/comments.
  - Current parser support remains syntax-limited; quoted identifiers and full PostgreSQL COPY grammar are not guaranteed.
  - `COPY FROM STDIN` enforces a maximum line size guardrail.
  - Evidence: `src/protocol/handler/dynamic/copy/mod.rs`, `src/protocol/handler/dynamic/copy/parse.rs`, `src/protocol/handler/tests.rs`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/main.rs`
- `src/protocol/handler/dynamic/mod.rs`
- `src/protocol/handler/dynamic/startup.rs`
- `src/protocol/handler/dynamic/query.rs`
- `src/protocol/handler/dynamic/copy/mod.rs`
- `src/protocol/handler/query_parser.rs`
- `src/protocol/handler/tenant.rs`
- `src/protocol/handler/portal.rs`
- `crates/pgwire/src/api/auth/mod.rs`
- `crates/pgwire/src/tokio/server.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/regression-gate`, `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`

## Change Management
- Any change to handshake semantics, username/keyspace parsing, Parse fallback behavior, suspended-portal buffering, or COPY handling MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking protocol changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368

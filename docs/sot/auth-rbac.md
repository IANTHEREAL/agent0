# auth-rbac — Authentication + RBAC model (current behavior)

## Scope
- Authentication bootstrap, password storage, and login eligibility.
- User/role DDL and privilege mutation (`CREATE ROLE`, `ALTER ROLE`, `DROP ROLE`, `GRANT`, `REVOKE`).
- Executor-visible privilege enforcement points and superuser-only boundaries.

## Non-goals
- pgwire message framing and startup packet details (authoritative: `./protocol-pgwire.md`).
- General SQL planning/execution semantics outside privilege checks (authoritative: `./sql-engine.md`).
- Persistent storage/key layout and tenant isolation invariants (authoritative: `./storage-format.md` and `./multi-tenancy.md`).
- Extension-specific runtime semantics beyond RBAC boundaries (authoritative: `./extensions-gin.md`).

## External Contracts
- **[Stable] Auth bootstrap is explicit and fail-closed**
  - If a tenant/keyspace has no superuser yet, db9 MUST bootstrap the initial superuser only when `DB9_BOOTSTRAP_ADMIN_PASSWORD` is set (optionally `DB9_BOOTSTRAP_ADMIN_USER`).
  - Outside `DB9_DEV=1`, missing bootstrap credentials on an uninitialized keyspace MUST fail closed.
  - Evidence: `src/auth/rbac.rs` (`AuthManager::bootstrap`), `src/protocol/handler/dynamic/startup.rs` (`authenticate_user`), `src/main.rs` (startup bootstrap for the default keyspace).

- **[Stable] Password hashing and verification**
  - Stored passwords MUST use `SHA-256(password || salt)` with a per-user random salt.
  - Password changes MUST rotate the salt.
  - Evidence: `src/auth/password.rs`, `src/auth/rbac.rs` (`User::new`, `User::set_password`, `User::verify_password`).

- **[Stable] Login eligibility**
  - Users with `can_login = false` MUST be rejected during authentication.
  - Evidence: `src/auth/rbac.rs` (`AuthManager::authenticate`), `src/protocol/handler/dynamic/startup.rs`.

- **[Stable] User/role DDL surface**
  - `CREATE ROLE` creates a stored identity and accepts the currently implemented option subset, including `LOGIN/NOLOGIN`, `PASSWORD`, `SUPERUSER`, `CREATEDB`, `CREATEROLE`, and `CONNECTION LIMIT`.
  - `ALTER ROLE ... WITH` applies the supported options, including password changes.
  - `DROP ROLE` removes the stored identity.
  - Evidence: `src/sql/rbac.rs`, `src/sql/executor/core/stmt_rbac.rs`, `tests/23_rbac.sql`.

- **[Stable] Privilege checks are wired, but coverage is partial**
  - Executor privilege enforcement uses `AuthManager::check_privilege()` via `Executor::require_privilege()` and `Executor::require_table_privilege()`.
  - Current call sites cover analyzed `SELECT/WITH`, `ANALYZE`, analyzed DML/prepared paths, and supported DDL/RBAC actions that explicitly call the helper layer.
  - Coverage is still partial relative to PostgreSQL: privilege storage/parsing is broader than the currently enforced executor surfaces, and some object kinds still collapse to coarse scopes.
  - Evidence: `src/sql/executor/core/statement.rs`, `src/sql/executor/core/analyze_rewrite.rs`, `src/sql/executor/core/stmt_dml.rs`, `src/sql/executor/core/stmt_ddl.rs`, `src/sql/executor/core/stmt_rbac.rs`, `src/sql/executor/core/dispatch/prepared.rs`, `src/auth/rbac.rs`, `src/sql/rbac.rs`.

- **[Stable] Superuser-only boundaries remain explicit**
  - Database-level DDL, extension management, and HTTP/embedding extension execution remain superuser-gated at the documented entrypoints.
  - Evidence: `src/sql/executor/core/stmt_ddl.rs`, `src/sql/executor/extensions.rs`, `src/extensions/http.rs`, `tests/86_extensions_framework_http.sql`, `tests/88_http_permission.sql`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/auth/password.rs`
- `src/auth/rbac.rs`
- `src/sql/rbac.rs`
- `src/sql/executor/core/statement.rs`
- `src/sql/executor/core/stmt_ddl.rs`
- `src/sql/executor/core/stmt_dml.rs`
- `src/sql/executor/core/stmt_rbac.rs`
- `src/protocol/handler/dynamic/startup.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/23_rbac.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/88_http_permission.sql`

## Change Management
- Any change to auth bootstrap defaults, password hashing format, privilege enforcement scope, or superuser-only boundaries MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking security changes require DR/ADR per #368 rules (impact surface, migration, rollback, and verification updates).
- Reference: https://github.com/c4pt0r/db9/issues/368

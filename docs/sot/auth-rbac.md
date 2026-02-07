# auth-rbac — Authentication + RBAC model (current behavior)

## Scope
- Authentication primitives (users/passwords) and password hashing/verification.
- Role/user DDL support (`CREATE ROLE`, `ALTER ROLE`, `DROP ROLE`) and privilege mutation (`GRANT`, `REVOKE`) as implemented.
- Security defaults and permission boundaries for built-in features (e.g., superuser-only actions).

## Non-goals
- pgwire wire-level auth message framing (authoritative: `./protocol-pgwire.md`).
- SQL semantics not directly related to auth/privileges (authoritative: `./sql-engine.md`).
- Storage encoding/key layout and tenant isolation invariants (authoritative: `./storage-format.md`).
- Extension feature contracts (authoritative: `./extensions-gin.md` for extension surfaces).

## External Contracts
- **[Stable] Auth bootstrap: default admin user**
  - On first authentication attempt for a tenant/keyspace, the server MUST bootstrap a default superuser user `admin` with password `admin` if it does not already exist.
  - Evidence: `src/auth/rbac.rs` (`AuthManager::bootstrap`, `DEFAULT_ADMIN_USER/PASSWORD`), `src/protocol/handler/dynamic.rs` (`authenticate_user` calls `bootstrap`).
  - Security note: this is a security-sensitive default; changes require DR/ADR per #368.

- **[Stable] Password hashing/verification (persistent)**
  - Passwords MUST be stored as `SHA-256(password || salt)` with a per-user random salt (hex-encoded).
  - Password updates MUST rotate the salt (generate a new salt).
  - Evidence: `src/auth/password.rs`, `src/auth/rbac.rs` (`User::new`, `User::set_password`, `User::verify_password`).

- **[Stable] Login eligibility**
  - Users with `can_login = false` MUST be denied authentication (current behavior: authentication fails with a fatal error).
  - Evidence: `src/auth/rbac.rs` (`AuthManager::authenticate`), `src/protocol/handler/dynamic.rs` (`authenticate_user` error path).

- **[Stable] Role/user DDL surface (as implemented)**
  - `CREATE ROLE` MUST create a user record and accept at least: `LOGIN/NOLOGIN`, `PASSWORD`, `SUPERUSER`, `CREATEDB`, `CREATEROLE`, `CONNECTION LIMIT`.
  - `ALTER ROLE ... WITH` MUST apply supported options (including `PASSWORD`).
  - `DROP ROLE` MUST remove the stored identity (best-effort supports both user and role records).
  - Evidence: `src/sql/rbac.rs` (parsing + mutations), `src/sql/executor/core.rs` (statement dispatch), `tests/23_rbac.sql`.

- **[Experimental] GRANT/REVOKE privilege storage vs enforcement**
  - `GRANT`/`REVOKE` MUST mutate stored privilege state for known privilege/action mappings (e.g., `SELECT/INSERT/UPDATE/DELETE/...`).
  - Table/object coverage is limited (e.g., some object variants are mapped to `Global`; multi-table lists are not fully expanded).
  - Privilege enforcement beyond coarse superuser checks is currently **TBD** (no call sites for `AuthManager::check_privilege` on the baseline).
  - Evidence: `src/sql/rbac.rs` (`parse_privileges`, `parse_privilege_object`), `src/auth/rbac.rs` (`check_privilege`), `rg -n \"check_privilege\\(\" -S src` (no usages), `tests/23_rbac.sql` (mutation coverage).

- **[Stable] Superuser-only boundaries (examples)**
  - Database-level DDL (`CREATE/DROP/ALTER DATABASE`) MUST be denied for non-superusers.
  - Extension management (`CREATE/DROP EXTENSION`) MUST be denied for non-superusers.
  - HTTP extension execution MUST be denied for non-superusers by default.
  - Evidence: `src/sql/executor/database.rs` (`session.is_superuser()` checks), `src/sql/executor/extensions.rs`, `src/extensions/http.rs` (`context::is_superuser`), `tests/88_http_permission.sql` + `tests/88_http_permission.errors`.

## Configuration
This module currently defines no module-specific runtime config keys.

If you need tenant selection or protocol/security-related env vars, they are defined exactly once in `./ops-config.md` (cross-link only).

## Entrypoints
- `src/auth/password.rs`
- `src/auth/rbac.rs` (`AuthManager`, `User`, `Role`, `_sys_user_` / `_sys_role_` prefixes)
- `src/sql/rbac.rs`
- `src/protocol/handler/dynamic.rs` (`authenticate_user`)

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn \"$PG_DSN\" tests/23_rbac.sql`
  - `python3 scripts/integration_test.py --dsn \"$PG_DSN\" tests/88_http_permission.sql`

## Change Management
- Any change to password hashing format, auth bootstrap defaults, or superuser-only boundaries MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes to security defaults require DR/ADR per #368 (impact surface + migration + rollback + verification updates).
- Reference: https://github.com/c4pt0r/tipg/issues/368

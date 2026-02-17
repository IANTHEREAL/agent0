# pg-tikv v0.1.0 Release Notes

## Summary

v0.1.0 ships a PostgreSQL wire-protocol frontend backed by TiKV (transactional API), with keyspace-based isolation for multi-tenancy.

## In Scope (v0.1.0)

- PostgreSQL wire-protocol frontend (`psql` + common drivers work)
- TiKV transactional storage backend (PD endpoint required)
- Basic DDL/DML (`CREATE TABLE`, `INSERT`, `UPDATE`, `DELETE`, `SELECT`)
- Basic transactions (`BEGIN` / `COMMIT` / `ROLLBACK`)
- Multi-tenancy via TiKV keyspaces (connect as `<keyspace>.<user>`)

## Out of Scope / Non-Goals (v0.1.0)

- Not a full PostgreSQL implementation; many features are not implemented yet
- Not production-hardened (see **Security Warning** and **Known Limitations** below)
- No `pg_catalog` / system catalogs (some ORMs/tools may break)
- No server-side connection limits, query timeouts, or HA/replication features

## Defaults & Configuration

> Note: this document describes **v0.1.0**. Newer versions have hardened defaults (loopback bind, explicit bootstrap, TLS posture). For the current config keys and defaults, see `docs/sot/ops-config.md`.

- **Listen address**: `0.0.0.0` (all interfaces)
- **Listen port**: `PG_PORT=5433`
- **PD endpoints**: `PD_ENDPOINTS=127.0.0.1:2379` (comma-separated)
- **Default keyspace**: `default` (maps to TiKV/PD built-in `DEFAULT` keyspace)
- **Default superuser**: `admin` / `admin`

Supported server env vars (v0.1.0):

- `PD_ENDPOINTS`: PD endpoints (comma-separated)
- `PG_PORT`: PostgreSQL protocol listen port
- `PG_KEYSPACE`: Default keyspace when the username has no `<keyspace>.` prefix
- `PGTIKV_TOKIO_STACK_MB`: Tokio worker thread stack size in MB (default: `4`)
- `PG_TLS_CERT` + `PG_TLS_KEY`: Enable TLS for pgwire connections (both required)

## Security Warning (Read Before Exposing)

**Do not expose pg-tikv to the public internet with defaults.** In v0.1.0:

- The server binds to `0.0.0.0:${PG_PORT}` (all interfaces).
- Each keyspace bootstraps a default superuser `admin` with password `admin`.

Minimum production hardening checklist:

1. **Change the default password (per keyspace)**:
   - Connect as `admin` (or `<keyspace>.admin`) and run: `ALTER ROLE admin WITH PASSWORD 'strong_random_password';`
2. **Enable TLS for pgwire**:
   - Set `PG_TLS_CERT=/path/to/server.crt` and `PG_TLS_KEY=/path/to/server.key` (PEM; PKCS#8 or RSA key).
   - Use a client option like `sslmode=require` (e.g. `psql "postgresql://admin:...@host:5433/postgres?sslmode=require"`).
3. **Restrict network exposure**:
   - There is no listen-address env var in v0.1.0; use firewall rules / private networking / container networking to limit access.

## Quick Start Notes

See `docs/quickstart.md`. Two details matter for a smooth first run:

1. **TiKV must run with keyspace support**: pg-tikv uses TiKV keyspaces by default, which requires API v2.
   - With TiUP playground, pass a TiKV config containing `storage.api-version = 2`.
2. **Extra tenants require keyspaces**: if you connect as `tenant_a.admin`, the `tenant_a` keyspace must already exist.
   - Create keyspaces via PD HTTP API (`/pd/api/v2/keyspaces`) or a pd-ctl equivalent.

## Rollback Guidance

v0.1.0 is the first tagged release and is intended as a developer preview. If you need to rollback:

1. Stop the running `pg-tikv` process.
2. Restart the previous `pg-tikv` binary with the same `PD_ENDPOINTS` / `PG_PORT` / `PG_KEYSPACE`.
3. Data lives in TiKV (per keyspace). Keep PD/TiKV running during rollback.

If you hit metadata incompatibilities between builds, the safest rollback is to:

- Switch to a fresh keyspace (e.g. set `PG_KEYSPACE=rollback_test` and connect without a `<keyspace>.` prefix), or
- Restore TiKV from a known-good backup (or wipe local dev data and start fresh).

## Known Limitations / Skips

- **ORM coverage**: `run_tests.sh` runs a subset of `orm-tests/` (pg client, TypeORM, Sequelize, Knex, Drizzle).
  - **Prisma is not run by `run_tests.sh`**, and `orm-tests/prisma` is not shipped in this repository at the moment. Prisma compatibility is not guaranteed in v0.1.0.
  - `kysely/` is also not currently included in the default `run_tests.sh` ORM subset.
  - **TypeORM gate skip**: `UNNEST(...)` is not supported yet. The v0.1.0 gate currently skips `TypeORM SQL Features [pg-tikv] ARRAY operations should support UNNEST`.
- **Passwords are sent in cleartext unless TLS is enabled** (use `PG_TLS_CERT` + `PG_TLS_KEY` in production).

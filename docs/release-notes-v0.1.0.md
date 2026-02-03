# pg-tikv v0.1.0 Release Notes

## Summary

v0.1.0 ships a PostgreSQL wire-protocol frontend backed by TiKV (transactional API), with keyspace-based isolation for multi-tenancy.

## Defaults & Configuration

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

## Quick Start Notes

See `docs/quickstart.md`. Two details matter for a smooth first run:

1. **TiKV must run with keyspace support**: pg-tikv uses TiKV keyspaces by default, which requires API v2.
   - With TiUP playground, pass a TiKV config containing `storage.api-version = 2`.
2. **Extra tenants require keyspaces**: if you connect as `tenant_a.admin`, the `tenant_a` keyspace must already exist.
   - Create keyspaces via PD HTTP API (`/pd/api/v2/keyspaces`) or a pd-ctl equivalent.

## Known Limitations / Skips

- **ORM coverage**: `run_tests.sh` runs a subset of `orm-tests/` (pg client, TypeORM, Sequelize, Knex, Drizzle).
  - **Prisma is currently not run by `run_tests.sh`**, and `orm-tests/prisma` is not shipped in this repository at the moment. Prisma compatibility is not guaranteed in v0.1.0.

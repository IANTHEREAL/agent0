# Getting Started

> **Source paths:** `src/main.rs`, `src/cli.rs`, `src/config.rs`, `src/pool.rs`, `Cargo.toml`

This guide covers building db9-server from source, configuring the server, connecting with a PostgreSQL client, and running the test suites.

---

## Prerequisites

- **Rust** (edition 2021) -- install via [rustup](https://rustup.rs/)
- **TiKV cluster** -- a running TiKV cluster with PD (Placement Driver) accessible at a known endpoint
- **Python 3** -- required for running integration tests (`scripts/integration_test.py`)
- **Node.js + npm** -- required for ORM compatibility tests (`orm-tests/`)
- **psql** -- PostgreSQL client for connecting to the server

---

## Build

```bash
# Standard build
cargo build

# Release build (optimized)
cargo build --release

# Run unit tests
cargo test
```

The project uses a vendored `tikv-client` crate (located at `vendor/tikv-client`) that adds `pessimistic_lock_wait_timeout` support for `SKIP LOCKED` / `NOWAIT` semantics.

---

## Configuration Reference

db9-server is configured through a combination of CLI flags and environment variables. CLI flags take precedence over environment variables, which take precedence over built-in defaults.

### CLI Flags

Defined in `src/cli.rs`. Supports both `--flag value` and `--flag=value` syntax.

| Flag | Description | Default | Env Variable |
|------|-------------|---------|-------------|
| `--host <ADDR>` | Listen address | `127.0.0.1` | `PG_LISTEN_ADDR` |
| `--port <PORT>` | Listen port | `5433` | `PG_PORT` |
| `--pd-endpoints <ENDPOINTS>` | PD endpoints (comma-separated) | `127.0.0.1:2379` | `PD_ENDPOINTS` |
| `--keyspace <NAME>` | Default TiKV keyspace | `default` | `PG_KEYSPACE` |
| `--tls-cert <PATH>` | TLS certificate file path | (none) | `PG_TLS_CERT` |
| `--tls-key <PATH>` | TLS private key file path | (none) | `PG_TLS_KEY` |
| `-h`, `--help` | Print help and exit | | |
| `-V`, `--version` | Print version and exit | | |

### Environment Variables

#### Server Configuration (`src/config.rs`)

| Variable | Description | Default |
|----------|-------------|---------|
| `DB9_STATEMENT_TIMEOUT_MS` | Maximum execution time per statement (milliseconds). `0` disables the timeout. | `60000` (60s) |
| `DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS` | Maximum idle time within an open transaction before the session is terminated (milliseconds) | `60000` (60s) |
| `DB9_MAX_CONNECTIONS` | Maximum concurrent client connections. Must be > 0. | `1000` |

#### Runtime Environment (`src/main.rs`)

| Variable | Description | Default |
|----------|-------------|---------|
| `DB9_TOKIO_STACK_MB` | Tokio worker thread stack size in megabytes. Increase if you encounter stack overflows on deeply nested queries. | `8` |
| `PG_REQUIRE_TLS` | When set to `1`/`true`/`yes`/`on`, the server refuses to start unless TLS is configured | `false` |
| `DB9_DEV` | Enable development mode. Allows non-TLS on non-loopback addresses. **Do not use in production.** | `false` |
| `DB9_INSECURE` | Explicitly allow insecure pgwire posture. **Do not use in production.** | `false` |
| `RUST_LOG` | Tracing filter (e.g., `info`, `debug`, `db9_server=debug`) | `info` |

#### Multi-Tenancy (`src/pool.rs`)

| Variable | Description | Default |
|----------|-------------|---------|
| `DB9_TENANT_QPS_LIMIT` | Per-tenant queries-per-second limit. `0` disables. | `0` |
| `DB9_TENANT_MEMORY_QUOTA_BYTES` | Per-tenant aggregate memory quota in bytes. `0` disables the gate — **no pod-OOM protection** (see #2555). | `1073741824` (1 GiB) |

#### fs9 WebSocket (`src/main.rs`)

| Variable | Description | Default |
|----------|-------------|---------|
| `FS9_WS_PORT` | fs9 WebSocket server port. `0` disables. | (built-in default) |
| `FS9_WS_LISTEN_ADDR` | fs9 WebSocket listen address | (built-in default) |

---

## Running the Server

### Minimal startup (local development)

```bash
# Start with a local TiKV cluster on default PD endpoint
PD_ENDPOINTS=127.0.0.1:2379 cargo run
```

### With explicit configuration

```bash
cargo run -- \
  --host 0.0.0.0 \
  --port 5433 \
  --pd-endpoints pd1:2379,pd2:2379 \
  --keyspace myapp
```

### With TLS

```bash
cargo run -- \
  --tls-cert /path/to/cert.pem \
  --tls-key /path/to/key.pem \
  --host 0.0.0.0
```

### Security model

The server enforces the following security posture (defined in `src/main.rs`):

- **Loopback addresses** (`127.0.0.1`, `::1`, `localhost`): TLS is optional.
- **Non-loopback addresses**: TLS is required unless `DB9_INSECURE=1` or `DB9_DEV=1` is set.
- **`PG_REQUIRE_TLS=1`**: The server refuses to start if TLS is not configured.

---

## Connecting via psql

Once the server is running, connect using any PostgreSQL client:

```bash
# Connect to default keyspace
psql -h 127.0.0.1 -p 5433 -U admin

# Connect to a specific keyspace (multi-tenancy)
psql -h 127.0.0.1 -p 5433 -U mykeyspace.admin
```

The username format for multi-tenancy is `<keyspace>.<user>`. The server parses this in `src/protocol/handler/tenant.rs` to route the connection to the correct TiKV keyspace.

---

## Running Tests

### Unit Tests

```bash
cargo test
```

Runs all Rust unit tests across the codebase. Key test modules include:
- `src/sql/analyzer/tests.rs` -- Analyzer correctness
- `src/sql/optimizer/*/tests.rs` -- Optimizer pipeline tests
- `src/sql/operators/*/tests.rs` -- Operator behavior tests
- `src/config.rs` -- Configuration parsing tests
- `src/cli.rs` -- CLI argument parsing tests

### SQL Integration Tests

```bash
python3 scripts/integration_test.py
```

Runs 287 SQL test files located in `tests/`. Each test consists of:
- A `.sql` file with SQL statements
- A `.expected` file with expected output (primary validation mode)
- Optionally a `.errors` file or `.assert` file (alternative validation modes)

Only one validation mode is used per test, with priority: `.expected` > `.errors` > `.assert`.

**Important contract:** Expected outputs must be validated against PostgreSQL 17.7 before being committed. Never guess what PostgreSQL returns.

### Regression Gate

```bash
./scripts/regression_gate.sh
./scripts/regression_gate.sh --skip-orm  # Skip ORM tests
```

Runs the full regression suite including integration tests.

### ORM Compatibility Tests

```bash
cd orm-tests && npm test
```

Tests compatibility with TypeORM, Prisma, and Sequelize ORMs.

### Clippy (Linting)

```bash
cargo clippy
```

---

## Development Workflow

### Typical change cycle

1. Identify the module boundary where the change belongs (see [Architecture Overview](Architecture-Overview.md))
2. Read the relevant `AGENTS.md` navigation doc (e.g., `src/sql/AGENTS.md`) to find entry points
3. Make the change
4. Run `cargo test` to verify unit tests pass
5. Run `python3 scripts/integration_test.py` to verify integration tests
6. If changing behavior or contracts, update `docs/sot/` and test expectations

### Adding a SQL function

1. Implement the function in the appropriate category under `src/sql/expr/functions/` (e.g., `string.rs`, `math.rs`, `json.rs`)
2. Register the type signature in `src/sql/types/registry/` (matching category file)
3. Add integration tests in `tests/`

### Adding a catalog view

1. Create a new file in `src/sql/catalog/` implementing the `VirtualTable` trait
2. Register it in `src/sql/catalog/mod.rs` (`CatalogRegistry`)
3. Add integration tests

### Adding a physical operator

1. Implement the `PhysicalOperator` trait in `src/sql/operators/` with `open()`, `next()`, `close()` methods
2. Wire it into the operator builder in `src/sql/optimizer/build/`
3. Add unit tests in the operator module and integration tests in `tests/`

---

## Project Conventions

- **Error handling:** Use `SqlError` with PostgreSQL-compatible SQLSTATE codes (defined in `src/sql/error.rs`)
- **Testing contract:** Always validate expected outputs against real PostgreSQL 17.7 before committing changes to `.expected` files
- **Documentation tiers:** Normative contracts in `docs/sot/`, descriptive architecture in `docs/architecture/`, navigation in `src/*/AGENTS.md`
- **No hidden fallbacks:** If a code path exists, it is the only path. No "try-new-then-old" patterns.

---

## See Also

- [Home](Home.md) -- Wiki navigation and quick reference
- [Architecture Overview](Architecture-Overview.md) -- System design and execution pipeline
- `docs/ARCHITECTURE.md` -- Canonical architecture document
- `docs/sot/ops-config.md` -- Normative configuration contract
- `docs/sot/testing-gates.md` -- Testing gate specification

---

*Source files: `src/main.rs`, `src/cli.rs`, `src/config.rs`, `src/pool.rs`, `Cargo.toml`*

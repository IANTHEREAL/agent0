# Testing Manual (Local + CI Alignment)

**Written**: 2026-02-06

> Applicable repo: `pg-tikv` (repo name: `tipg`)  
> Goal: help you quickly answer three questions: **what tests exist, what CI runs, and how to run them locally and reproduce**; and provide a method to "use PostgreSQL as an oracle to validate test cases".

---

## 0) TL;DR (Recommended Order)

1. **Fastest regression gate** (target < 5 min): `bash scripts/regression_gate.sh`
2. **Full local suite** (slower but widest coverage): `./run_tests.sh`
3. **Single-case repro**: `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/NNN_case.sql`

---

## 1) Tests Overview: What exists, and what does each guarantee?

| Category | Entry/Path | Main Purpose | Typical Usage |
|---|---|---|---|
| Rust unit tests | `cargo test` | Core logic regression (parsing/execution/types/protocol, etc.) | `cargo test` |
| Built-in integration tests (lightweight, default in CI) | `scripts/integration_test.py` | Quick validation: connection/DDL/DML/transactions/JSON/query features (does not depend on `tests/*.sql`) | `python3 scripts/integration_test.py --dsn "$PG_DSN"` |
| SQL integration tests (golden case corpus) | `tests/` + `scripts/integration_test.py` | End-to-end "pgwire + SQL behavior + output formatting" (compares against `.expected` / `.assert` / `.errors`) | `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/` |
| Fast regression gate (gate pack) | `scripts/regression_gate.sh` + `scripts/regression_gate.list` | A small, deterministic pack of high-frequency regressions for fast iteration | `bash scripts/regression_gate.sh` |
| ORM compatibility tests (Vitest) | `orm-tests/` | Real driver/ORM query-path compatibility (TypeORM/Sequelize/Knex/Drizzle/pg-client, etc.) | `cd orm-tests && PG_DSN=... npm test -- typeorm/` |
| E2E smoke (Tier2) | `e2e/` + `bash scripts/e2e_tests.sh ...` | Smoke suites closer to real apps: GORM, SQLAlchemy, Dify-lite | `PG_DSN=... bash scripts/e2e_tests.sh sqlalchemy_smoke` |
| Script-style focused tests (optional) | `scripts/test_write_conflict_retry.py`, etc. | Validate specific semantics/bugs (e.g. WriteConflict retry) | `pip install psycopg2-binary && python3 scripts/test_write_conflict_retry.py --dsn "$PG_DSN"` |

### 1.0 Two modes of `scripts/integration_test.py` (Important)

- **Without any `tests/` args**: runs *Built-in integration tests* (8 small tests). CI uses this by default.  
  `python3 scripts/integration_test.py --dsn "$PG_DSN"`
- **With `tests/` or a single `.sql` file**: runs the *golden SQL case corpus* (compares against `.expected/.assert/.errors`).  
  `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/`

### 1.1 SQL integration test file structure (`tests/*.sql`)

Conventions in `scripts/integration_test.py`:

- `tests/NNN_xxx.sql`: input SQL (executed via `psql -f`)
- `tests/NNN_xxx.expected`: **exact output match** (the runner normalizes decimals/timestamps/JSON whitespace/array literals, etc.)
- `tests/NNN_xxx.assert`: **substring assertions** (each non-empty line must appear in the output; useful for unstable cases)
- `tests/NNN_xxx.errors`: **allowed error patterns** (use when the case is expected to error)
- `tests/NNN_xxx_setup.sql`: setup before running the main case (e.g. create DB/create tables)
- `tests/NNN_xxx_load.py`: data-loading script before running the main case (internally uses `psql`)
- The runner writes actual output to `tests/NNN_xxx.out` (useful for `diff`)

Output formatting:
- If `.expected` is `|`-separated (unaligned), the runner uses `psql -P format=unaligned -P fieldsep=| -P null=NULL`
- If `.expected` is in the default "table style" (aligned), the runner uses default `psql` aligned output

---

## 2) What does CI run? What isn't in CI but is recommended?

### 2.1 Current CI coverage (see `.github/workflows/`)

- `orm-tests.yml` (main workflow)
  - `cargo test` (Rust unit tests)
  - Start TiKV (`tiup playground`) + start `pg-tikv`
  - `python3 scripts/integration_test.py --dsn ...` (Built-in integration; **does not run the `tests/` case corpus**)
  - `npm test` (ORM: first `|| true`, then gate via a "failure count threshold" to avoid CI being permanently red due to known limitations)
  - `cargo clippy`; `cargo fmt --check` (currently `fmt-check` is `continue-on-error: true`)

- `regression-gate.yml`
  - `./scripts/regression_gate.sh` (fast gate: SQL pack from `tests/` + a small ORM subset; SSOT=`scripts/regression_gate.list`)

- `gorm-smoke.yml`
  - `bash scripts/e2e_tests.sh gorm_smoke` (currently `continue-on-error: true`; designed as "observe stability first, then upgrade to required")

- `sqlalchemy-smoke.yml`
  - `bash scripts/e2e_tests.sh sqlalchemy_smoke`
  - `bash scripts/e2e_tests.sh dify_sqlalchemy_compat`

### 2.2 Tests not in CI / not enforced but recommended to add (by priority)

1. **Bring the full `tests/` (golden SQL case corpus) into CI (recommended as Nightly/manual trigger)**  
   Current state: CI only runs the Built-in integration tests + the SQL pack in regression gate.  
   Suggested shape: `workflow_dispatch` or nightly (avoid slowing PRs), and promote *critical regressions* into `scripts/regression_gate.list`.

2. **PostgreSQL oracle validation (validate that cases/expected are correct)**  
   Goal: prevent `.expected` from drifting into "implementation-detail output" instead of "PostgreSQL semantics".  
   Suggested shape: `workflow_dispatch` or nightly (avoid taking too long).

3. **Tiered inclusion of `tests_pending/`**  
   Suggested approach:
   - First promote stable/deterministic ones into `tests/` or `scripts/regression_gate.list`
   - Or create a separate `nightly-tests-pending.yml`

4. **Upgrade `gorm-smoke` from non-blocking to required (after it stabilizes)**  
   The workflow already documents a promotion path; consider "green for N consecutive days" as the promotion condition.

5. **Add Sequelize regressions to the fast gate (or extend the ORM pack)**  
   Current state: the `[orm]` section of `scripts/regression_gate.list` covers `pg-client` + a TypeORM subset; Sequelize regressions may be missed.  
   Suggestion: add `orm-tests/sequelize/connection.test.ts` (or a smaller `showIndex` regression) into `[orm]` to cover introspection behaviors like `pg_catalog.pg_index.indkey`.

6. (Optional) Make `cargo fmt -- --check` a required gate  
   Currently, `fmt-check` in `orm-tests.yml` is non-blocking; make it blocking if you want enforced formatting.

---

## 3) Validate test-case correctness using PostgreSQL (oracle)

> Goal: answer "does this `.sql + .expected` test PostgreSQL semantics?"  
> Method: use the same runner (`scripts/integration_test.py`) but point the DSN to PostgreSQL.

### 3.1 Install PostgreSQL (Ubuntu/Debian)

```bash
sudo apt-get update
sudo apt-get install -y postgresql postgresql-client
```

Start the service:

```bash
sudo systemctl enable --now postgresql
```

### 3.2 Create a test account (match pg-tikv defaults)

```bash
sudo -u postgres psql -v ON_ERROR_STOP=1 <<'SQL'
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'admin') THEN
    CREATE ROLE admin LOGIN PASSWORD 'admin' CREATEDB;
  END IF;
END $$;
SQL
```

On some PostgreSQL 17+ distributions, the default `public` schema privileges may be tightened. If you see `permission denied for schema public`, run:

```bash
sudo -u postgres psql -v ON_ERROR_STOP=1 -c "GRANT ALL ON SCHEMA public TO admin;"
```

### 3.3 Run oracle validation

> Note: not every case is guaranteed to pass 100% on vanilla PostgreSQL (some may contain pg-tikv-specific behavior/error text). Start with "basic compatibility" cases first.

```bash
export PG_DSN_PG='postgres://admin:admin@127.0.0.1:5432/postgres'
python3 scripts/integration_test.py --dsn "$PG_DSN_PG" tests/01_ddl_basic.sql
python3 scripts/integration_test.py --dsn "$PG_DSN_PG" tests/02_dml_crud.sql
```

Tip: the runner writes `tests/*.out` (ignored in `.gitignore`). If you run the same `.sql` against different backends (pg-tikv vs PostgreSQL), `.out` will be overwritten. Save it elsewhere if you need evidence.

If you want to validate whether a case’s `.expected` matches PostgreSQL output:

- Pass: the case/expected is consistent with PostgreSQL (at least for that dimension)
- Fail:
  - First confirm whether the diff is due to output formatting (aligned/unaligned) or normalizable differences (the runner already normalizes multiple classes of output)
  - Then decide whether it is a **semantic difference between pg-tikv and PostgreSQL** or the **case/expected is wrong**

---

## 4) Local runbook: How to run each class of tests (pg-tikv)

### 4.1 Dependency check (local)

Recommended minimum:

- Rust + Cargo (stable)
- Node.js (>= 18; CI uses 20)
- Python (>= 3.10) + `uv`
- `tiup` (to start TiKV playground)
- `postgresql-client` (provides `psql` / `pg_isready`; required by the SQL runner)

### 4.2 Start/stop TiKV (recommended: use the admin script)

```bash
uv run scripts/tikv_admin.py start --name dev --persistent
uv run scripts/tikv_admin.py list
uv run scripts/tikv_admin.py stop --name dev
uv run scripts/tikv_admin.py clean --name dev
```

The output will contain `PD_ENDPOINTS=127.0.0.1:<port>`; use it to start `pg-tikv`.

Note: `scripts/tikv_admin.py start` uses PD client port `2379` by default (and PD peer port `2380`). If you already have a cluster occupying these ports, it may "look like it started successfully but actually reused the old PD". Recommended:

- Run `uv run scripts/tikv_admin.py list` to inspect existing clusters and stop/clean what you don't need.
- Or explicitly specify a different free port: `uv run scripts/tikv_admin.py start --name dev --pd-port <free_port> --persistent`

### 4.3 Start pg-tikv

```bash
cargo build --release
PD_ENDPOINTS=127.0.0.1:<pd_port> \
PG_PORT=5433 \
PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=admin \
./target/release/pg-tikv
```

Connect (requires `psql`):

```bash
export PG_DSN='postgres://admin:admin@127.0.0.1:5433/postgres'  # password comes from bootstrap
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres -c 'SELECT 1'
```

### 4.4 Rust unit tests

```bash
cargo test
```

### 4.5 SQL integration tests (golden)

CI-style (Built-in, small set):

```bash
python3 scripts/integration_test.py --dsn "$PG_DSN"
```

Run all:

```bash
python3 scripts/integration_test.py --dsn "$PG_DSN" tests/
```

Run a single file:

```bash
python3 scripts/integration_test.py --dsn "$PG_DSN" tests/01_ddl_basic.sql
```

Common options:

```bash
python3 scripts/integration_test.py --help
python3 scripts/integration_test.py --dsn "$PG_DSN" --verbose tests/01_ddl_basic.sql
python3 scripts/integration_test.py --dsn "$PG_DSN" --stop-on-error tests/
```

### 4.6 Fast Regression Gate (recommended before/for every change)

Default (automatically runs `cargo test` + starts TiKV + builds release + starts pg-tikv + runs the gate pack):

```bash
bash scripts/regression_gate.sh
```

Reuse an existing pg-tikv (faster):

```bash
bash scripts/regression_gate.sh --dsn "$PG_DSN"
```

SQL-only pack:

```bash
bash scripts/regression_gate.sh --skip-orm
```

Gate pack SSOT: `scripts/regression_gate.list`

### 4.7 ORM tests (Vitest)

```bash
cd orm-tests
npm ci
PG_DSN="$PG_DSN" npm test
```

Run a single ORM suite:

```bash
PG_DSN="$PG_DSN" npm test -- typeorm/
PG_DSN="$PG_DSN" npm test -- sequelize/
```

Run a specific test file (commonly used for regression localization):

```bash
PG_DSN="$PG_DSN" npm test -- typeorm/transaction.test.ts
```

### 4.8 E2E smoke (Tier2)

```bash
PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh gorm_smoke
PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh sqlalchemy_smoke
PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh dify_sqlalchemy_compat
```

### 4.9 Full suite (one command: TiKV + build + integration + ORM)

```bash
./run_tests.sh
```

Reports will be saved under `test-reports/test-report-*.md`.

---

## 5) Common issues (Troubleshooting)

1. `psql: command not found` / `pg_isready: command not found`  
   - Install: `sudo apt-get install -y postgresql-client`

2. `tiup: command not found` or TiUP mirror flake  
   - First confirm `~/.tiup/bin` is in PATH  
   - CI already has cache/retry; locally it is recommended to reuse a persistent `TIUP_HOME` (default `~/.tiup`)

3. Port conflicts (`PG_PORT` / `PD`)  
   - `scripts/regression_gate.sh` auto-picks a free `PG_PORT` when `PG_PORT` is not explicitly set
   - Manual override example: `PG_PORT=15433 ...`
   - If `PD` ports (2379/2380) are already in use, stop the existing TiKV clusters or specify `--pd-port` when starting TiKV via `scripts/tikv_admin.py`

---

## 6) Verification record for this document (Filled by Maintainer)

> Goal: avoid "the doc commands don’t run". Record the commands you actually ran and key evidence here (update as needed).

- Repo: `c4pt0r/tipg`

### 2026-02-06 (upstream maintainer)

- Commit: `610679f` (`master`)
- Host: `instance-20260126-142742`
- Commands executed (local evidence):
  - `cargo test` ✅ (803 tests passed)
  - `bash scripts/regression_gate.sh` ✅ (SQL pack: 4 passed; ORM pack: 121 passed; report dir: `test-reports/regression-gate-20260206-093311/`)
  - `python3 scripts/integration_test.py --dsn "$PG_DSN"` ✅ (Built-in integration: 8 passed; executed after starting a temporary `pg-tikv`)
  - `./run_tests.sh` ❌ (Integration golden: 159 passed / 10 failed; ORM: Sequelize 3 suites fail; full report: `test-reports/test-report-20260206-093943.md`)
  - PostgreSQL oracle ✅ (installed `postgresql` + `postgresql-client`; created `admin/admin` and ran `GRANT ALL ON SCHEMA public`; passed: `tests/01_ddl_basic.sql`, `tests/02_dml_crud.sql`)
  - `PG_DSN=... bash scripts/e2e_tests.sh gorm_smoke` ❌ (currently failing: `record not found`; in CI this workflow is currently non-blocking)

### 2026-02-06 (local re-check for PR #450)

- Commit: `1a67876` (PR #450 head)
- Host: `instance-20260126-142742`
- Commands executed (local evidence):
  - `cargo test` ✅ (803 passed)
  - `bash scripts/regression_gate.sh` ✅ (SQL pack: 4 passed; ORM pack: 121 passed; report dir: `test-reports/regression-gate-20260206-102239/`)
  - `python3 scripts/integration_test.py --dsn "$PG_DSN"` ✅ (Built-in integration: 8 passed; ran against a temporary `pg-tikv`)
  - `./run_tests.sh` ❌ (Integration golden: 158 passed / 11 failed; ORM: Sequelize 3 suites fail; report: `test-reports/test-report-20260206-102557.md`)
  - PostgreSQL oracle ✅ (passed: `tests/01_ddl_basic.sql`, `tests/02_dml_crud.sql`)
  - `PG_DSN=... bash scripts/e2e_tests.sh gorm_smoke` ❌ (`record not found`)

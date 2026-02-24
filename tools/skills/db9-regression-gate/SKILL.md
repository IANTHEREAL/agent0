---
name: db9-regression-gate
description: "Run db9-server test gates quickly (fast regression gate + full suite) and produce a standard evidence snippet for PRs."
---

# db9 regression gate (fast + full)

Run commands from the repo root (example: `cd /path/to/db9`).

## Prerequisites (minimal)

- `cargo` (Rust toolchain)
- `uv` (runs `scripts/tikv_admin.py`)
- `tiup` (starts local TiKV)
- `pg_isready` (PostgreSQL client utils)
- `node` + `npm` (required for `./run_tests.sh` ORM suite)

## Fast gate (default for PRs)

Runs unit tests + a curated SQL regression set against a fresh TiKV + db9-server instance:

```bash
bash scripts/regression_gate.sh
```

Useful options:

```bash
# Reuse an existing running db9-server instance (skip starting TiKV/db9-server):
bash scripts/regression_gate.sh --dsn "$PG_DSN"

# More verbose output + stop on first SQL failure:
bash scripts/regression_gate.sh -v -x
```

## Full suite (slower; pre-merge confidence)

Runs integration tests + ORM compatibility tests and writes a Markdown report:

```bash
./run_tests.sh
```

Look for the line: `Report saved to: /path/to/db9/test-reports/test-report-YYYYMMDD-HHMMSS.md` (absolute path; relative location is `test-reports/test-report-YYYYMMDD-HHMMSS.md`)

## Evidence snippet (copy/paste into PR)

Fill in the report path printed by `./run_tests.sh` (or omit if you only ran the fast gate):

```text
Test gates:
- fast: bash scripts/regression_gate.sh (exit 0)
- full: ./run_tests.sh (exit 0; report: test-reports/test-report-YYYYMMDD-HHMMSS.md)
Commit: $(git rev-parse --short HEAD)
```

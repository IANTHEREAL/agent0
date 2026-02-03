---
name: tipg-regression-gate
description: "Run pg-tikv test gates quickly (fast regression gate + full suite) and produce a standard evidence snippet for PRs."
---

# tipg regression gate (fast + full)

Run commands from the repo root (example: `cd /path/to/tipg`).

## Prerequisites (minimal)

- `cargo` (Rust toolchain)
- `uv` (runs `scripts/tikv_admin.py`)
- `tiup` (starts local TiKV)
- `pg_isready` (PostgreSQL client utils)
- `node` + `npm` (required for `./run_tests.sh` ORM suite)

## Fast gate (default for PRs)

Runs unit tests + a curated SQL regression set against a fresh TiKV + pg-tikv instance:

```bash
bash scripts/regression_gate.sh
```

Useful options:

```bash
# Reuse an existing running pg-tikv instance (skip starting TiKV/pg-tikv):
bash scripts/regression_gate.sh --dsn "$PG_DSN"

# More verbose output + stop on first SQL failure:
bash scripts/regression_gate.sh -v -x
```

## Full suite (slower; pre-merge confidence)

Runs integration tests + ORM compatibility tests and writes a Markdown report:

```bash
./run_tests.sh
```

Look for the line: `Report saved to: /path/to/tipg/test-reports/test-report-YYYYMMDD-HHMMSS.md` (absolute path; relative location is `test-reports/test-report-YYYYMMDD-HHMMSS.md`)

## Evidence snippet (copy/paste into PR)

Fill in the report path printed by `./run_tests.sh` (or omit if you only ran the fast gate):

```text
Test gates:
- fast: bash scripts/regression_gate.sh (exit 0)
- full: ./run_tests.sh (exit 0; report: test-reports/test-report-YYYYMMDD-HHMMSS.md)
Commit: $(git rev-parse --short HEAD)
```

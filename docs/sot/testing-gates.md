# testing-gates — CI merge gates & local reproducibility

## Scope
- CI workflows/jobs that act as merge gates (and what they run).
- Local reproducibility: how to run the same checks locally.
- Gate identifiers used by `docs/sot/modules.yaml` (`gate_tests`) and what they mean.

## Non-goals
- Feature semantics (authoritative: the corresponding module SoT doc).
- Adding/removing/tuning gates without DR/ADR (see #368 hard rule).
- Defining runtime configuration keys (SSOT lives in `./ops-config.md`).

## Entrypoints
- `.github/workflows/regression-gate.yml`
- `.github/workflows/orm-tests.yml`
- `.github/workflows/gorm-smoke.yml`
- `.github/workflows/sqlalchemy-smoke.yml`
- `.github/workflows/doc-lint.yml`
- `scripts/regression_gate.sh`
- `run_tests.sh`
- `scripts/integration_test.py`
- `scripts/e2e_tests.sh`
- `scripts/doc_lint.py`

## CI Gates

Evidence baseline: these gates are derived from `.github/workflows/**` job definitions (see entrypoints above).

| Gate ID (used in registry) | Blocking | Category | What it runs (high-level) | Local reproduce |
|---|---:|---|---|---|
| `ci:.github/workflows/orm-tests.yml/lint` | Required | Structure | `cargo fmt -- --check` *(non-blocking)* + `cargo clippy` | `cargo fmt -- --check && cargo clippy` |
| `ci:.github/workflows/orm-tests.yml/test` | Required | Correctness | build + unit tests + bring up TiKV + start `db9-server` + run `scripts/integration_test.py tests/**` + run ORM suites (best-effort thresholded) | `./run_tests.sh` |
| `ci:.github/workflows/regression-gate.yml/regression-gate` | Required | Correctness | bring up TiKV + start `db9-server` + run `scripts/regression_gate.sh` (SQL regression packs; optional ORM pack) | `./scripts/regression_gate.sh` |
| `ci:.github/workflows/doc-lint.yml/doc-lint` | Required | Structure | `uv run scripts/doc_lint.py` (SoT registry/doc drift prevention) | `uv run scripts/doc_lint.py` |
| `ci:.github/workflows/gorm-smoke.yml/gorm-smoke` | Best-effort | Smoke | start TiKV + `db9-server` + run `bash scripts/e2e_tests.sh gorm_smoke` *(step is `continue-on-error: true`)* | `PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh gorm_smoke` |
| `ci:.github/workflows/sqlalchemy-smoke.yml/sqlalchemy-smoke` | Required | Smoke | start TiKV + `db9-server` + run `bash scripts/e2e_tests.sh sqlalchemy_smoke` + `bash scripts/e2e_tests.sh dify_sqlalchemy_compat` | `PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh sqlalchemy_smoke && bash scripts/e2e_tests.sh dify_sqlalchemy_compat` |

Blocking definition (evidence-first):
- **Required**: the job does not use `continue-on-error` for its core checks (it can fail the workflow run).
- **Best-effort**: the job (or its core test step) is marked `continue-on-error: true` and provides signal without blocking merges.

## Local Repro

Minimal local reproduction commands (>= 3):

```bash
# 1) Required: fast regression gate
./scripts/regression_gate.sh

# 2) Required: full suite (build + unit + integration + ORM)
./run_tests.sh

# 3) Required (structure): lint
cargo fmt -- --check && cargo clippy

# 4) Required (docs): SoT doc-lint
uv run scripts/doc_lint.py
```

Optional smoke signals (best-effort):

```bash
# Requires a running db9-server instance; set connection DSN accordingly.
PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh gorm_smoke
PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh sqlalchemy_smoke
```

Notes:
- Runtime env keys for starting `db9-server` (e.g. `PD_ENDPOINTS`, `PG_PORT`, TLS env) are SSOT in `./ops-config.md`.
- Some scripts assume local tooling (`tiup`, `psql/pg_isready`, `python3`, `uv`, `node/npm`, `go`) — see each script’s help and the workflow steps as evidence.

## SQL Validation Modes (`scripts/integration_test.py`)

`scripts/integration_test.py` is the authoritative test-harness behavior for `tests/*.sql` companion files.

Mode contract (current behavior):
- `.expected` = full output snapshot mode. If present, harness compares normalized query output against `.expected` and returns immediately on pass/fail.
- `.errors` = expected error substring mode. If present (and `.expected` is absent), harness requires at least one SQL diagnostic line and validates each against allowed patterns.
- `.assert` = required output fragment mode. If present (and `.expected` is absent), harness requires each assertion needle to appear in output.

Combination rules:
- `.expected` MUST be treated as exclusive snapshot mode for a test case.
- `.errors + .assert` is an allowed composite mode when a test must assert both error pattern(s) and additional output fragment(s) in the same run.
- `.errors` file MUST NOT be empty.

PostgreSQL parity rule:
- Before changing any `.expected`, `.errors`, or `.assert` contract for SQL semantics, run the corresponding `.sql` against real PostgreSQL 17.7 and record parity evidence in the PR.

Test annotation rule (compatibility clarity):
- New or changed SQL tests that define behavior contracts SHOULD include a top-of-file marker comment:
  - `PG_PARITY`: expected to match PostgreSQL behavior.
  - `DB9_DIVERGENCE(<issue-or-adr-id>)`: intentional divergence with explicit governance link.
- `DB9_DIVERGENCE(...)` tests MUST NOT be merged without a linked SoT section describing rationale, user value, and impact boundary.

Marker examples:
```sql
-- PG_PARITY: verify SQLSTATE and result shape match PostgreSQL 17.7
```

```sql
-- DB9_DIVERGENCE(#1421): explicit-transaction extension visibility uses transaction-consistent semantics
```

Evidence bundle rule (required for disputed semantics):
- Include PostgreSQL version (`SELECT version()`), exact reproduction script, raw SQLSTATE/output, execution date, and db9 counterpart output in the PR.
- For concurrency semantics, evidence MUST use at least two independent sessions.

## Verification (Gates)
- CI: run the **Required** gates listed in `## CI Gates`.
- Local: use the commands in `## Local Repro` to reproduce the same checks outside CI.
- Registry linkage: module → gate references live in `docs/sot/modules.yaml` (`gate_tests`); this doc defines what each referenced gate ID means.

## Change Management
- Any change to a CI workflow/job, script path, or “blocking vs best-effort” behavior MUST update this document and the referenced `gate_tests` in `docs/sot/modules.yaml`.
- If you add/remove/relax/tighten a gate, create a DR/ADR per #368 rules and record:
  - why the gate changed,
  - how to reproduce locally,
  - rollback plan (how to re-enable/restore the previous signal).
- If a gate introduces new runtime config requirements, document the config keys in `./ops-config.md` (do not redefine them here).
- Reference: https://github.com/c4pt0r/db9/issues/368

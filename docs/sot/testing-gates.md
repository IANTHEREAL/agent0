# testing-gates — Repository workflow inventory and local reproducibility

## Scope
- Repository workflows/jobs that provide blocking or best-effort signal.
- Canonical gate identifiers used by `docs/sot/modules.yaml`.
- Local commands for reproducing the same checks outside GitHub Actions where possible.

## Non-goals
- Feature semantics (authoritative: the corresponding module SoT docs).
- Claiming GitHub branch-protection settings that are not versioned in-repo.
- Duplicating runtime config-key definitions (authoritative: `./ops-config.md`).

## Entrypoints
- `.github/workflows/ci.yml`
- `.github/workflows/doc-lint.yml`
- `.github/workflows/governance-lint.yml`
- `scripts/regression_gate.sh`
- `run_tests.sh`
- `scripts/integration_test.py`
- `scripts/e2e_tests.sh`
- `scripts/doc_lint.py`

## CI Gates

Repository truth model:
- **Workflow-blocking** means a failing job fails its workflow run as defined in the workflow YAML.
- **Best-effort** means the workflow intentionally allows its core signal to continue on error.
- This document inventories repository-defined jobs only; it does not assert GitHub branch-protection "required checks".

| Gate ID | Workflow effect | Category | What it runs | Local reproduce |
|---|---|---|---|---|
| `ci:.github/workflows/ci.yml/lint` | Workflow-blocking | Structure | `cargo fmt -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` | `cargo fmt -- --check && cargo clippy --workspace --all-targets -- -D warnings` |
| `ci:.github/workflows/ci.yml/unit-tests` | Workflow-blocking | Correctness | `cargo test` | `cargo test` |
| `ci:.github/workflows/ci.yml/regression-gate` | Workflow-blocking | Correctness | Release build artifact + TiKV + `./scripts/regression_gate.sh --skip-unit --skip-build` | `./scripts/regression_gate.sh` |
| `ci:.github/workflows/ci.yml/integration-tests` | Workflow-blocking | Correctness | Release build artifact + TiKV + `python3 scripts/integration_test.py` over `tests/**` + `orm-tests` npm suite | `./run_tests.sh` |
| `ci:.github/workflows/ci.yml/gorm-smoke` | Best-effort | Smoke | TiKV + db9 + `bash scripts/e2e_tests.sh gorm_smoke` with `continue-on-error: true` on the smoke step | `PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh gorm_smoke` |
| `ci:.github/workflows/ci.yml/sqlalchemy-smoke` | Workflow-blocking | Smoke | TiKV + db9 + `bash scripts/e2e_tests.sh sqlalchemy_smoke` + `bash scripts/e2e_tests.sh dify_sqlalchemy_compat` | `PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres' bash scripts/e2e_tests.sh sqlalchemy_smoke && PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres' bash scripts/e2e_tests.sh dify_sqlalchemy_compat` |
| `ci:.github/workflows/doc-lint.yml/doc-lint` | Workflow-blocking | Documentation | `uv run scripts/doc_lint.py` | `uv run scripts/doc_lint.py` |
| `ci:.github/workflows/governance-lint.yml/governance-lint` | Workflow-blocking | Governance | PR-body / label / issue-link / size-policy checks via GitHub API | No full local equivalent; reproduce in GitHub Actions or by manually evaluating the workflow script against a PR payload. |

## Local Repro

Typical local commands:

```bash
cargo fmt -- --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test
./scripts/regression_gate.sh
./run_tests.sh
uv run scripts/doc_lint.py
```

Optional smoke commands against a running db9 instance:

```bash
PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres?sslmode=disable' bash scripts/e2e_tests.sh gorm_smoke
PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres' bash scripts/e2e_tests.sh sqlalchemy_smoke
PG_DSN='postgres://admin:admin@127.0.0.1:<port>/postgres' bash scripts/e2e_tests.sh dify_sqlalchemy_compat
```

## SQL Validation Modes (`scripts/integration_test.py`)

`scripts/integration_test.py` is the authoritative harness contract for `tests/*.sql`.

Mode contract:
- `.expected` = exclusive full-output snapshot mode.
- `.errors` = expected diagnostic substring mode.
- `.assert` = required output fragment mode.

Combination rules:
- `.expected` is exclusive snapshot mode.
- `.errors + .assert` is allowed when a single test needs both diagnostics and output fragments.
- `.errors` files MUST NOT be empty.

PostgreSQL parity rule:
- Before changing `.expected`, `.errors`, or `.assert` for SQL semantics, run the same `.sql` against PostgreSQL 17.7 and record the evidence.

Test annotation rule:
- New or changed SQL tests that define behavior SHOULD include one of:
  - `PG_PARITY`
  - `DB9_DIVERGENCE(<issue-or-adr-id>)`

Evidence bundle rule for disputed semantics:
- Include PostgreSQL version, exact reproduction script, raw SQLSTATE/output, execution date, and db9 counterpart output.
- Concurrency semantics require at least two independent client sessions.

## Verification (Gates)
- CI gate identifiers are defined in `## CI Gates`.
- Module-to-gate linkage lives in `docs/sot/modules.yaml`.
- Local reproduction commands in `## Local Repro` are the standard baseline outside CI.

## Change Management
- Any change to repository workflow/job inventory, workflow-blocking vs best-effort semantics, or local reproduction commands MUST update this document and the affected `gate_tests` entries in `docs/sot/modules.yaml`.
- If a gate changes scope or strictness, document the reason, local reproduce path, and rollback plan.
- Reference: https://github.com/c4pt0r/db9/issues/368

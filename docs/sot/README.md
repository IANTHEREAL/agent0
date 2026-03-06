# SoT (Source of Truth) — Module Registry & Map

This directory (`docs/sot/**`) is the authoritative SSOT for current user-facing behavior/contracts and the metadata that keeps those contracts from drifting.

The machine-readable registry is `docs/sot/modules.yaml`. This README is the human map.

## SoT tier rules

- Only `docs/sot/**` is SoT.
- `docs/**` outside `docs/sot/` are descriptive, historical, or tutorial material and may drift.
- Any PR that changes behavior/contracts MUST update the relevant SoT docs or explicitly justify `SoT-Impact: None`.

## Normative language and stability labels

- **MUST / MUST NOT**: required contract
- **SHOULD / SHOULD NOT**: strong recommendation; deviations require justification
- **MAY**: optional behavior

Stability labels used inside SoT docs:
- **Stable**: breaking changes require DR/ADR plus migration, rollback, and gate updates
- **Experimental**: behavior may still change, but it must be labeled and evidence-backed
- **Deprecated**: still supported for now; removal requires a migration path

## PostgreSQL Parity Adjudication Pattern

When behavior is intended to match PostgreSQL, disputes MUST be resolved with a reproducible PostgreSQL oracle flow, not by memory or test-harness artifacts.

Rules:
- PostgreSQL 17.7 is the canonical parity baseline for SQL behavior and SQLSTATE adjudication.
- Every parity dispute MUST record exact reproduction commands and observed outputs in the PR discussion or linked artifact.
- Concurrency or visibility disputes MUST use at least two independent client sessions.
- Oracle selection MUST be surface-consistent: function-call contracts are validated through that same function-call surface, not a different catalog observation path.
- If PostgreSQL evidence and current SoT diverge, the change MUST either:
  - align code/tests with PostgreSQL, or
  - declare an intentional divergence with rationale, scope, impact, verification, and governance link.

## Compatibility Strategy: PG-Compatible by Default, DB9-Better by Explicit Design

The default product strategy is:
- PostgreSQL compatibility first for SQL behavior, SQLSTATE contracts, and protocol-visible semantics.
- Intentional divergence is allowed only when it delivers clear DB9 value, especially around distributed architecture, safety, operability, or performance.

Every accepted divergence MUST document:
- `Why not PG here`
- `User value`
- `Behavior delta`
- `Scope`
- `Fallback or control`
- `Verification`
- `Exit criteria`
- `Governance`

## No overlaps without cross-links

Module boundaries may touch, but contracts MUST have exactly one authoritative home.

Rules:
- If a module doc needs another module's contract, it MUST link to the authoritative doc instead of restating the contract.
- Cross-module touch points MUST be recorded through `xref` in `docs/sot/modules.yaml`.

## SoT map

The table below mirrors `docs/sot/modules.yaml`. Every module marked `PARTIAL` or `TBD` must state the gap and next step.

| Module | SoT doc | Code entrypoints (evidence) | Owner | Gate tests (evidence) | Status | Gaps / next step |
|---|---|---|---|---|---|---|
| `protocol-pgwire` | `./protocol-pgwire.md` | `src/protocol/handler/dynamic/mod.rs`<br>`src/protocol/handler/query_parser.rs`<br>`src/protocol/handler/tenant.rs`<br>`src/protocol/handler/portal.rs` | `TBD` | `ci.yml:regression-gate`<br>`ci.yml:integration-tests` | `PARTIAL` | Confirm owners; keep Parse fallback, portal buffering, and COPY coverage aligned with SoT. |
| `sql-engine` | `./sql-engine.md` | `src/sql/executor/core/analyze_rewrite.rs`<br>`src/sql/rewriter/mod.rs`<br>`src/sql/executor/core/dispatch/prepared.rs`<br>`src/sql/executor/core/plan_cache.rs` | `TBD` | `ci.yml:regression-gate`<br>`ci.yml:integration-tests` | `PARTIAL` | Confirm owners; add focused coverage for remaining PG-parity disputes and observability pseudo-tables. |
| `storage-format` | `./storage-format.md` | `src/storage/encoding/mod.rs`<br>`src/storage/encoding/serialization.rs`<br>`src/storage/tikv_store/mod.rs`<br>`src/sql/hnsw/storage.rs` | `TBD` | `ci.yml:regression-gate`<br>`ci.yml:integration-tests`<br>`cargo test` | `PARTIAL` | Confirm owners; keep SoT at invariant/key-family level and push detailed inventories into code. |
| `catalog-introspection` | `./catalog-introspection.md` | `src/sql/information_schema.rs`<br>`src/sql/catalog/mod.rs`<br>`src/sql/catalog/virtual_tables.rs`<br>`src/sql/catalog_oids.rs` | `TBD` | `ci.yml:integration-tests` | `PARTIAL` | Confirm owners; expand only the catalog matrixes that materially clarify shipped behavior. |
| `auth-rbac` | `./auth-rbac.md` | `src/auth/rbac.rs`<br>`src/sql/rbac.rs`<br>`src/sql/executor/core/statement.rs`<br>`src/protocol/handler/dynamic/startup.rs` | `TBD` | `ci.yml:integration-tests` | `PARTIAL` | Confirm owners; privilege enforcement is wired but broader object coverage is still incomplete. |
| `extensions-gin` | `./extensions-gin.md` | `src/extensions/http.rs`<br>`src/extensions/embedding.rs`<br>`src/sql/gin.rs`<br>`src/sql/planner/gin_predicate.rs`<br>`src/sql/operators/gin_scan.rs` | `TBD` | `ci.yml:integration-tests` | `PARTIAL` | Confirm owners; keep security-sensitive behavior and embedding SQLSTATE edges explicit and tested. |
| `testing-gates` | `./testing-gates.md` | `.github/workflows/ci.yml`<br>`.github/workflows/doc-lint.yml`<br>`.github/workflows/governance-lint.yml`<br>`scripts/regression_gate.sh`<br>`scripts/doc_lint.py` | `TBD` | `ci.yml:*`<br>`doc-lint.yml:doc-lint`<br>`governance-lint.yml:governance-lint` | `PARTIAL` | Confirm owners; extend doc-lint incrementally without conflating workflow inventory and branch protection. |
| `ops-config` | `./ops-config.md` | `src/cli.rs`<br>`src/main.rs`<br>`src/config.rs`<br>`src/worker/config.rs`<br>`src/cron/config.rs`<br>`src/storage/backpressure.rs` | `TBD` | `ci.yml:lint`<br>`ci.yml:integration-tests`<br>`doc-lint.yml:doc-lint` | `PARTIAL` | Confirm owners; keep config definitions centralized here and add dedicated TLS/security posture smoke coverage later. |
| `worker-cron` | `./worker-cron.md` | `src/worker/engine.rs`<br>`src/worker/gc.rs`<br>`src/worker/types.rs`<br>`src/cron/parser.rs`<br>`src/cron/worker.rs` | `TBD` | `ci.yml:regression-gate`<br>`ci.yml:integration-tests` | `PARTIAL` | Confirm owners; expand focused HNSW background-merge coverage as needed. |
| `multi-tenancy` | `./multi-tenancy.md` | `src/protocol/handler/tenant.rs`<br>`src/protocol/handler/dynamic/startup.rs`<br>`src/pool.rs` | `TBD` | `ci.yml:regression-gate`<br>`ci.yml:integration-tests`<br>`cargo test` | `PARTIAL` | Confirm owners; add direct tenant-isolation coverage when new tenant-scoped state appears. |

> Note: [invariants.md](./invariants.md) contains cross-module system invariants. It is part of the SoT directory but is not a module row in `modules.yaml`.

## Update workflow

1. Update `docs/sot/modules.yaml` first.
2. Keep this SoT map aligned with the registry.
3. Update the corresponding module SoT doc with explicit `Scope`, `Non-goals`, `Entrypoints`, `Verification (Gates)`, and `Change Management`.
4. If you add, remove, tighten, or relax a gate, update `testing-gates.md` and record the governance change.

# SoT (Source of Truth) — Module Registry & Map

This directory (`docs/sot/**`) is the **authoritative SSOT** for *current* user-facing behavior/contracts and their drift-prevention metadata (ownership + gates).

The machine-readable registry is `docs/sot/modules.yaml`. This README is the human index (“SoT map”).

## SoT tier rules

- Only `docs/sot/**` is **SoT**.
- `docs/**` (outside `docs/sot/`) are design/notes/tutorials and may drift.
- Any PR that changes behavior/contracts MUST update the relevant SoT doc(s) (or explicitly declare `SoT-Impact: None` with justification).

## Normative language & stability labels (conventions)

- **MUST / MUST NOT**: required contract
- **SHOULD / SHOULD NOT**: strong recommendation; deviations must be justified
- **MAY**: optional behavior

Stability labels inside SoT docs:
- **Stable**: breaking changes require DR/ADR + migration + rollback + gate updates
- **Experimental**: may change; must be labeled and gated
- **Deprecated**: still supported but scheduled for removal; must include migration path

## PostgreSQL Parity Adjudication Pattern (cross-module)

When behavior is intended to match PostgreSQL, disputes MUST be resolved with a reproducible PostgreSQL oracle flow, not by local assumption or test-harness artifacts.

Rules:
- PostgreSQL 17.7 is the canonical parity baseline for SQL behavior and SQLSTATE adjudication.
- Every parity dispute MUST record exact reproduction commands and observed outputs in PR discussion (or linked artifact), including absolute versions and timestamps.
- Concurrency or visibility disputes MUST use at least two independent client connections (for example psycopg with two sessions). `psql` scripts with `\\!` subprocess calls MAY be used only as supporting evidence, not as the sole oracle.
- Oracle selection MUST be surface-consistent: if the contract under review is a function call, parity evidence MUST be built from that function-call path (same SQL surface), not from a different catalog-observation path.
- A single user-visible contract MUST map to one semantic visibility source. Implementations MUST NOT mix independent visibility timelines (for example, transaction snapshot for one path and latest-committed read for another path) without an explicit SoT contract describing precedence.
- If PostgreSQL reproduction and current SoT text diverge, the PR MUST either:
  - update code/tests to match PostgreSQL, or
  - explicitly declare intentional divergence with rationale, migration impact, and DR/ADR reference.

## Compatibility Strategy: PG-Compatible by Default, DB9-Better by Explicit Design

The default product strategy is:
- PostgreSQL compatibility first for SQL behavior, SQLSTATE contracts, and protocol-observable semantics.
- Intentional divergence is allowed only when it delivers clear user value for DB9 (for example distributed architecture constraints, safety, operability, or performance).

Intentional divergence contract (MUST for every accepted divergence):
- `Why not PG here`: concrete reason PG parity is not chosen now.
- `User value`: explicit improvement vs PG for DB9 users.
- `Behavior delta`: exact SQL-visible difference and SQLSTATE/wire impact.
- `Scope`: affected modules, SQL surfaces, and versions.
- `Fallback or control`: feature flag/GUC/config if applicable.
- `Verification`: parity tests, divergence tests, and reproducible evidence.
- `Exit criteria`: conditions and owner plan to converge to PG later (if planned).
- `Governance`: linked DR/ADR and issue(s) for tracking.

## No overlaps without cross-links

Module boundaries may touch, but **contracts MUST have exactly one authoritative home**.

Rules:
- If a module doc needs to mention another module’s contract, it MUST link to the authoritative section and MUST NOT restate the contract.
- Overlaps MUST be explicitly tracked via `xref` in `docs/sot/modules.yaml`.

## SoT map (coverage proof)

The table below is derived from `docs/sot/modules.yaml`. Every module marked `PARTIAL`/`TBD` MUST state the gap(s) and the next step to reach `FULL`.

| Module | SoT doc | Code entrypoints (evidence) | Owner | Gate tests (evidence) | Status | Gaps / next step |
|---|---|---|---|---|---|---|
| `protocol-pgwire` | `./protocol-pgwire.md` | `src/protocol/handler/dynamic/mod.rs`<br>`src/protocol/handler/tenant.rs`<br>`src/protocol/handler/portal.rs`<br>`crates/pgwire/src/tokio/server.rs`<br>`crates/pgwire/src/messages/codec.rs` | `TBD` | `orm-tests.yml:test`<br>`regression-gate.yml:regression-gate` | `PARTIAL` | Add CODEOWNERS; confirm owners; keep protocol SoT aligned with gate coverage. |
| `sql-engine` | `./sql-engine.md` | `src/sql/parser/mod.rs`<br>`src/sql/executor/core/mod.rs`<br>`src/sql/expr/typed_eval/mod.rs` | `TBD` | `orm-tests.yml:test`<br>`regression-gate.yml:regression-gate` | `PARTIAL` | Add CODEOWNERS; add explicit gate for observability sys tables. |
| `storage-format` | `./storage-format.md` | `src/storage/encoding/mod.rs`<br>`src/storage/tikv_store/mod.rs`<br>`src/pool.rs` | `TBD` | `orm-tests.yml:test`<br>`regression-gate.yml:regression-gate` | `PARTIAL` | Add CODEOWNERS; reduce non-SoT doc drift by cross-linking to SoT. |
| `catalog-introspection` | `./catalog-introspection.md` | `src/sql/information_schema.rs`<br>`src/sql/catalog/mod.rs`<br>`src/sql/catalog/pg_type.rs` | `TBD` | `orm-tests.yml:test` | `PARTIAL` | Add CODEOWNERS; expand catalog matrix and record ORM-facing gaps/next steps. |
| `auth-rbac` | `./auth-rbac.md` | `src/auth/rbac.rs`<br>`src/sql/rbac.rs`<br>`src/protocol/handler/dynamic/startup.rs` | `TBD` | `orm-tests.yml:test` | `PARTIAL` | Add CODEOWNERS; define GRANT/REVOKE enforcement scope and add regression coverage. |
| `extensions-gin` | `./extensions-gin.md` | `src/extensions/http.rs`<br>`src/sql/gin.rs`<br>`src/sql/fts.rs` | `TBD` | `orm-tests.yml:test` | `PARTIAL` | Add CODEOWNERS; keep security-sensitive behavior explicit and gated (HTTP/SSRF + GIN/FTS). |
| `testing-gates` | `./testing-gates.md` | `.github/workflows/orm-tests.yml`<br>`.github/workflows/regression-gate.yml`<br>`.github/workflows/doc-lint.yml`<br>`scripts/regression_gate.sh`<br>`scripts/doc_lint.py` | `TBD` | `orm-tests.yml:lint`<br>`orm-tests.yml:test`<br>`regression-gate.yml:regression-gate`<br>`doc-lint.yml:doc-lint` | `PARTIAL` | Confirm owners; keep `doc-lint` gate stable; extend checks incrementally (config key uniqueness, xref coverage). |
| `ops-config` | `./ops-config.md` | `src/main.rs`<br>`src/tls.rs`<br>`docs/configuration.md` | `TBD` | `orm-tests.yml:lint`<br>`orm-tests.yml:test` | `PARTIAL` | Confirm owners; add TLS handshake gate + config-key uniqueness checks. |
| `worker-cron` | `./worker-cron.md` | `src/worker/engine.rs`<br>`src/worker/gc.rs`<br>`src/cron/parser.rs`<br>`src/cron/worker.rs` | `TBD` | `orm-tests.yml:test`<br>`regression-gate.yml:regression-gate` | `PARTIAL` | Add CODEOWNERS; confirm owners. |
| `multi-tenancy` | `./multi-tenancy.md` | `src/protocol/handler/tenant.rs`<br>`src/pool.rs`<br>`src/session_context.rs` | `TBD` | `orm-tests.yml:test`<br>`regression-gate.yml:regression-gate` | `PARTIAL` | Add CODEOWNERS; confirm owners. |

> **Note**: [invariants.md](./invariants.md) contains cross-module system invariants. It is not a module in `modules.yaml` but is part of the SoT directory.

## How to update (workflow)

1. Update `docs/sot/modules.yaml` first (registry is SSOT).
2. Keep this SoT map table in sync with the registry.
3. Create/update the module SoT doc at `doc_path` with explicit **Scope** / **Non-goals** and a non-empty **Entrypoints** section.
4. If you add/remove/relax/tighten a gate, create a DR/ADR (per #368 rules) and update `testing-gates`.

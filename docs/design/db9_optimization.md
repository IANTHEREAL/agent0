# db9 Server Performance Optimization Execution Plan

**Status**: Draft  
**Author**: AI Assistant  
**Date**: 2026-04-09  
**Scope**: Next-step development plan for performance optimization milestones `M0` to `M8`

> **Draft / non-SoT note**
>
> This document is a forward-looking implementation and delivery plan, not a current-behavior contract.
> Validate shipped behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, tests, and the implementation under `src/**`.

---

## Scope

- Primary repo: `db9-ai/db9-server`
- Storage repo: `tidbcloud/cloud-storage-engine`
- Team roles are restricted to only two kinds: `Architect` and `CI`.
- All optimization features must be executed strictly in serial order.
- Only one optimization feature may be in active development at a time.
- `Architect` and `CI` may overlap inside the current feature, but no coding for the next feature starts until the current feature passes its exit gate.

## Serial Execution Order

1. `M0` Builtin/function and operator pushdown wave, batch 1 in progress and batch 2 pending
2. `M1` Baseline, observability, protocol and rollback contract
3. `M2` End-to-end streaming execution
4. `M3` Covering or index-only scan plus late materialization
5. `M4` Ordered execution path, real TopN, and broader operator pushdown
6. `M5` Real StreamAggregate
7. `M6` Shared and generic plan cache
8. `M7` Parallel and distributed execution
9. `M8` Spill-to-disk and external execution

## Hard Rules

- Every `Architect` and `CI` task must follow the target repository `AGENTS.md` and any child `AGENTS.md` in the touched subtree.
- PostgreSQL is the semantic oracle for SQL behavior.
- No hidden fallback, no runtime `try-new-then-old`, and no test-layer masking.
- Performance optimization features should avoid `cloud-storage-engine` changes whenever reasonably possible. The default implementation target is a `db9-server`-only solution.
- All cross-repo protocol changes must be capability-gated and backward-aware.
- Every milestone must ship observability before enablement.
- Every milestone must define explicit rollback knobs or disable flags.
- `cloud-storage-engine` CI must not run multiple `cargo` commands concurrently.
- Any change in `cloud-storage-engine` must not affect, regress, or alter any existing TiDB to TiKV functionality, protocol contract, compatibility surface, or interface behavior outside the explicitly scoped DB9 extension path.
- If a milestone still needs `cloud-storage-engine` changes, the design doc must explain why a `db9-server`-only approach is insufficient and why the chosen `cloud-storage-engine` change is the smallest viable scope.
- Before any commit or PR is marked ready, the owner must ensure DCO requirements pass.
- Before any Rust change is submitted, the owner must run the repository-required formatting and lint steps and ensure they pass.
- For `cloud-storage-engine`, `make format` and `make clippy` are mandatory before submission for Rust-related changes.
- For `db9-server`, the submission gate must satisfy the repo-defined formatting and lint requirements from its `AGENTS.md` and CI workflow.
- `db9-server` SQL golden changes require PostgreSQL 17.7 validation evidence for semantics.
- Every optimization milestone must add or update a dedicated design document under `docs/design/` that explains how the optimization works, what scope it changes, what assumptions it relies on, and how it is rolled out and rolled back.
- Any optimization-introduced parameter, flag, GUC, environment variable, DDL option, or `cloud-storage-engine` runtime knob must be documented in `docs/performance-optimization-parameters.md` in the same milestone.
- If the new parameter is an operator-facing runtime input for `db9-server`, the same change must also update `docs/sot/ops-config.md` as the authoritative registry.
- Every optimization milestone must prove compatibility with all functions, expressions, and operators that are already supported for DB9 Cop pushdown. The baseline user-facing reference is `docs/sql/functions/list-of-expressions-for-pushdown.md`, and the planner or runtime implementation remains the final source of truth for what is supported.
- Baseline milestone `M1` defines the benchmark harness and observability baseline rather than shipping a standalone optimization feature.
- Every feature milestone `M0` and `M2` to `M8` must include at least one real scenario benchmark gate against local PostgreSQL 18.3.
- A milestone is not done until both `Architect-*` and `CI-*` backlogs are closed.

## Common Exit Gate For Every Milestone

- Code merged in the correct repo or repos with protocol compatibility documented.
- Dedicated design documentation exists under `docs/design/` for the milestone and reflects the shipped behavior.
- `EXPLAIN` or metrics can prove the new path is being used.
- Positive acceptance passes.
- Negative acceptance passes.
- Regression suite passes.
- Repository discipline checks pass, including `AGENTS.md` compliance, DCO, formatting, and lint requirements.
- All newly introduced optimization parameters are documented with scope, meaning, default, and usage method.
- `cloud-storage-engine` changes are proven not to change existing TiDB or TiKV behavior outside the DB9-scoped path.
- Any `cloud-storage-engine` change is justified as strictly necessary, and the evidence bundle explains why a `db9-server`-only implementation was not enough.
- Compatibility regression checks pass for all already-supported pushdown functions, expressions, and operators.
- Real scenario benchmark gate passes on production-like data volume.
- Performance evidence includes before and after numbers plus workload description.
- The evidence bundle includes a comparison against local PostgreSQL 18.3 on the same schema, data, and query shape.
- Rollback path is documented and tested.
- Open risks and non-goals are recorded before handoff to the next milestone.

## Evidence Bundle Required At Milestone Close

- Design note or ADR summary
- Dedicated optimization design doc path under `docs/design/`
- PR list in execution order
- Changed feature flags or capability bits
- Updated parameter documentation entry or explicit statement that no new optimization parameter was introduced
- DCO and repo-discipline check result summary
- Benchmark inputs and result table
- PostgreSQL parity evidence where semantics are affected
- Pushdown compatibility regression report covering already-supported functions, expressions, and operators
- Real scenario benchmark report with `db9 before`, `db9 after`, and `PostgreSQL 18.3` comparison
- Failure-mode evidence for cancel, memory pressure, and incompatible peers
- TiDB or TiKV non-regression evidence for any `cloud-storage-engine` change
- Final go or no-go recommendation for enabling by default

## M2-M4 Optimization Objectives

- `M2` must make streaming execution deliver real early-stop behavior. Once `LIMIT` is satisfied or the client stops consuming, `db9-server` and `cloud-storage-engine` should stop producing more rows as early as correctness allows.
- `M3` and `M4` must use composite indexes to serve filtering and preserve `ORDER BY` order at the same time whenever possible, so eligible queries avoid an explicit `Sort` operator.
- `M4` must make `ORDER BY ... LIMIT n OFFSET m` exploit ordered input for early stop. The target is to stop after `offset + limit` qualifying index entries, and to defer base-row fetch so that fetched full rows are as close to `limit` as possible.
- The common optimization purpose across `M2` to `M4` is to reduce the payload size between `db9-server` and `cloud-storage-engine`, reduce KV request count, reduce full-row decoding, and reduce unnecessary memory materialization.

## PostgreSQL Compatibility Work By Milestone

- `M0 Builtin/Function And Operator Pushdown`: compatibility work is mandatory. Pushdown must preserve PostgreSQL-visible function semantics, SQLSTATE surfaces, null handling, type coercion outcomes, and operator behavior. Only provably safe signatures and operator forms may be pushed, and unsupported forms must stay local.
- `M1 Baseline, Observability, Protocol, And Rollback Contract`: compatibility work is mostly meta and observability-focused. This milestone should not introduce new user-visible SQL behavior; it defines the contract and measurement baseline for later feature milestones.
- `M2 Streaming Execution`: compatibility work is required at the pgwire and executor boundary. Streaming must preserve PostgreSQL-visible row order, portal suspension behavior, cancel behavior, statement timeout behavior, and final command completion sequencing. No new PostgreSQL-style tuning parameter is required by default; this milestone is mainly about preserving PostgreSQL protocol and visible execution semantics while changing internals.
- `M3 Late Materialization And Index-Only`: compatibility work is required if covering-index capability extends DDL or planner behavior. If db9 adds stored payload columns for covering indexes, it should prefer PostgreSQL-compatible `CREATE INDEX ... INCLUDE (...)` syntax instead of inventing a DB9-only index-payload syntax. Any index-only or late-fetch path must preserve PostgreSQL-visible MVCC and row-visibility semantics even if the internal storage strategy differs from PostgreSQL heap plus visibility map.
- `M4 Ordered Path And TopN`: compatibility work is required for `ORDER BY`, `LIMIT`, and `OFFSET` semantics. Ordered execution must preserve PostgreSQL behavior for `ASC/DESC`, `NULLS FIRST/LAST`, collation-sensitive order, and the rule that `LIMIT/OFFSET` without `ORDER BY` does not guarantee deterministic row order. If planner debug controls are introduced for ordered-path testing, prefer PostgreSQL-style names and semantics such as `enable_sort` or related planner toggles before inventing DB9-only controls.
- `M5 StreamAggregate`: compatibility work is required for aggregate semantics, especially `GROUP BY`, `DISTINCT`, null handling, and finalization behavior. Internal stream aggregation is allowed to differ physically, but visible results must match PostgreSQL. If any planner-path debugging control is added, prefer PostgreSQL-compatible behavior modeled after existing planner toggles such as `enable_hashagg`.
- `M6 Shared Plan Cache`: compatibility work is mandatory. Reuse PostgreSQL-compatible `plan_cache_mode=auto|force_custom_plan|force_generic_plan` semantics for generic-versus-custom control. Keep `db9.shared_plan_cache_max_bytes` as a clearly DB9-specific resource-governance parameter rather than a PostgreSQL compatibility surface.
- `M7 Parallel And Distributed Execution`: compatibility work is required if user-visible parallel controls or observability are added. Prefer PostgreSQL-compatible controls and vocabulary where applicable, such as `max_parallel_workers_per_gather`, `parallel_setup_cost`, and `parallel_tuple_cost`, for local parallel planning behavior. Any DB9-specific distributed controls must use `db9.`-prefixed names. Result ordering must remain PostgreSQL-compatible, meaning unordered parallel paths still provide no order guarantee unless an explicit `ORDER BY` is present.
- `M8 Spill And External Execution`: compatibility work is required for memory and temp-file governance. Prefer PostgreSQL-compatible semantics and naming around `work_mem`, `hash_mem_multiplier`, and `temp_file_limit` where the behavior is analogous, and only add DB9-specific parameters when PostgreSQL has no matching control surface. Spill must not change visible SQL results; it only changes resource usage and execution strategy.

## Composite Index Strategy For db9-server

- For `M3` and `M4`, `db9-server` should adopt TiDB-style composite-index planning where the index column order is chosen to satisfy both filtering and ordering, not just one of them.
- Default composite-index priority for eligible queries should be: equality predicates, then `ORDER BY` columns, then range or multi-value predicates, then covering columns if index-only or late-materialization gains justify them.
- Planner must model left-prefix behavior explicitly. A composite index only preserves order for downstream `ORDER BY` if the leading columns are fixed in a way that does not break ordered traversal.
- Single-value equality predicates such as `=` and safe `IS NULL` checks may preserve ordered traversal for later sort keys. Multi-value `IN`, non-prefix ordering, incompatible mixed sort directions, unsupported collation rules, or earlier range predicates must not be labeled as order-preserving unless the implementation can prove correctness.
- When an eligible composite index can satisfy filter plus order, planner should prefer `ordered index scan -> optional offset skip -> limit/topn -> late fetch`, instead of `index scan -> full row fetch -> sort -> limit`.
- The practical evaluation criteria for `M3` and `M4` are not only latency. They must also show fewer KV requests, fewer bytes moved between `cloud-storage-engine` and `db9-server`, fewer decoded rows, and less sort memory.

## Real Scenario Benchmark Gate

- Feature milestones `M0` and `M2` to `M8` cannot close without at least one real scenario benchmark that matches a realistic application query shape.
- Default minimum dataset size is `100k` rows. If the feature is expected to help medium or large scans, sorts, aggregates, joins, or distributed work, the preferred dataset is `1M+` rows.
- The same benchmark harness must run three variants: `db9 before change`, `db9 after change`, and local `PostgreSQL 18.3`.
- All three variants must use the same schema, logically equivalent indexes, same data distribution, same query text or equivalent prepared statement shape, same client driver or harness, and documented server settings.
- The benchmark report must include at least: elapsed time, p50, p95, first-row latency when relevant, peak RSS, CPU time when available, and feature-specific counters such as fetched base rows, decoded rows, sort bytes, aggregate memory, spill bytes, or plan-cache hit ratio.
- `db9 after change` must show a measurable improvement versus `db9 before change` on the target scenario. If not, the milestone cannot close unless the feature is re-scoped as correctness-only and explicitly approved as an exception.
- Comparison to local `PostgreSQL 18.3` is a mandatory reference line. Beating PostgreSQL is not required, but any large gap must include a short root-cause explanation and the next bottleneck to address.
- All benchmark runs must include workload description, hardware description, dataset generator, server settings, query text, run count, warmup policy, and raw output attachment.

## Backlog Template

Use the following card structure for every assigned task:

- `Owner`: `Architect-*` or `CI-*`
- `Milestone`: one of `M0` to `M8`
- `Objective`: one sentence
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`, or specific subset
- `Inputs`: required documents, files, and existing constraints
- `Implementation or Test Backlog`: flat checklist
- `Deliverables`: code, tests, metrics, docs, rollout notes
- `Design Doc`: path under `docs/design/` and confirmation that it matches the shipped behavior
- `Parameter Docs`: whether `docs/performance-optimization-parameters.md` changed, and whether `docs/sot/ops-config.md` was required
- `Pushdown Compatibility`: coverage against all already-supported pushdown functions, expressions, and operators
- `Positive Acceptance`: expected success behavior
- `Negative Acceptance`: expected rejection, fallback, or failure behavior
- `Exit Evidence`: exact commands, reports, or screenshots to attach
- `Discipline Checks`: repo `AGENTS.md` items followed, DCO status, formatting status, lint status

## M0 Backlog

### Architect-00-Builtin-Function-And-Operator-Pushdown

- `Objective`: complete the ongoing first wave of builtin/function and operator pushdown as the currently active optimization project, while preserving semantic safety and keeping `cloud-storage-engine` changes minimal.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Inputs`: first-batch PRs already in progress:
- upstream merged protocol prerequisites:
- `kvproto` PR: [pingcap/kvproto#1439](https://github.com/pingcap/kvproto/pull/1439)
- `kvproto` PR: [pingcap/kvproto#1444](https://github.com/pingcap/kvproto/pull/1444)
- `cloud-storage-engine` PR: [tidbcloud/cloud-storage-engine#4876](https://github.com/tidbcloud/cloud-storage-engine/pull/4876)
- `db9-server` PR: [db9-ai/db9-server#2374](https://github.com/db9-ai/db9-server/pull/2374)
- second-batch PRs are expected after this first wave and should be linked here once opened
- `Implementation or Test Backlog`:
- finish the first-batch builtin/function pushdown and operator pushdown scope already under review
- keep pushdown eligibility signature-specific and operator-form-specific; unsupported cases must remain local
- preserve PostgreSQL-visible semantics, SQLSTATE surfaces, null handling, coercion behavior, and transaction visibility
- keep `cloud-storage-engine` changes scoped to the DB9 extension path and do not broaden TiDB/TiKV interfaces
- update `docs/sql/functions/list-of-expressions-for-pushdown.md` and this execution plan as the supported surface changes
- define at least one `100k+` row real scenario benchmark for the newly pushed function or operator shapes and compare `db9 before`, `db9 after`, and local `PostgreSQL 18.3`
- record the second-batch PR links and scope boundaries once they are opened
- `Deliverables`:
- merged first-batch pushdown PRs
- recorded upstream merged `kvproto` prerequisites and the exact protocol dependency they unlock
- updated pushdown support documentation
- compatibility and regression evidence for newly pushed functions/operators
- placeholder section for second-batch PR links and scope
- `Positive Acceptance`:
- newly supported functions/operators are pushed only when the planner/runtime can prove semantic safety
- `EXPLAIN` and regression tests show pushdown on eligible cases and local execution on ineligible cases
- the wave reduces payload bytes or KV requests on target query shapes without changing visible SQL results
- `Negative Acceptance`:
- unsupported signatures or operator forms must not be silently pushed
- any semantic drift versus PostgreSQL, wrong SQLSTATE surface, or visibility regression fails the milestone
- `cloud-storage-engine` changes that widen TiDB/TiKV behavior outside DB9 scope fail the milestone
- `Exit Evidence`:
- linked PRs for batch 1
- linked upstream merged `kvproto` prerequisites and the consumed revision or dependency bump
- updated pushdown whitelist/support report
- PostgreSQL oracle evidence and pushdown compatibility regression report
- real scenario benchmark report on `100k+` rows with `db9 before`, `db9 after`, and local `PostgreSQL 18.3`, including payload bytes or KV-request reduction on at least one target query shape

### CI-00-Builtin-Function-And-Operator-Pushdown

- `Objective`: validate the ongoing first wave of builtin/function and operator pushdown against semantic correctness, already-supported pushdown compatibility, and scoped DB9 Cop behavior.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Implementation or Test Backlog`:
- add or extend oracle-driven SQL cases for the new pushed builtin/functions and operators
- extend pushdown compatibility regression to cover new signatures and operator combinations introduced by batch 1
- verify unsupported forms still remain local and produce PostgreSQL-compatible results and errors
- verify `EXPLAIN` evidence for pushed versus local forms
- verify `cloud-storage-engine` endpoint behavior remains DB9-scoped and does not alter TiDB/TiKV paths
- add the real scenario benchmark gate for the pushed function or operator shapes on `100k+` rows, using the same harness against local `PostgreSQL 18.3`
- reserve a follow-up slot in the compatibility matrix for batch-2 PRs once they are opened
- `Positive Acceptance`:
- newly pushed functions/operators behave the same as local execution and PostgreSQL on supported cases
- unsupported cases stay local with correct behavior
- `Negative Acceptance`:
- false-positive pushdown, result drift, wrong errors, or TiDB/TiKV surface regression fail the milestone
- `Exit Evidence`:
- pushdown compatibility matrix update
- oracle regression results for newly pushed signatures/operators
- CSE non-regression evidence for DB9-scoped changes
- real scenario benchmark report with elapsed time, p95, payload bytes, and KV requests for `db9 before`, `db9 after`, and local `PostgreSQL 18.3`

## M1 Backlog

### Architect-01-Baseline-And-Contract

- `Objective`: freeze the execution contract and create the measurement baseline for all later milestones.
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`
- `Inputs`: current hot paths in `scan.rs`, `executor.rs`, `tables.rs`, `sort.rs`, `aggregate.rs`, `db9/mod.rs`, and `remote_dispatcher.rs`.
- `Implementation or Test Backlog`:
- define protocol capability bits for streaming, ordered execution, aggregate mode, remote limit, index-row-fetch mode, and spill support
- add or standardize metrics for scanned rows, decoded rows, fetched base rows, first-row latency, peak statement memory, sort memory, aggregate memory, spill bytes, plan-cache hit or miss, remote reject reasons, and DB9 streaming mode
- add feature flags or guarded planner switches for every later milestone
- document rollback rules and compatibility matrix for `old db9/new cse`, `new db9/old cse`, and `new/new`
- define a `db9-server`-first decision rule: each milestone must first attempt a `db9-server`-only design, and only escalate to `cloud-storage-engine` after documenting why the local-only design cannot meet correctness or target payoff
- freeze benchmark corpus and dataset shapes for small, medium, and large workloads
- define one real scenario benchmark template for each milestone and the common output format for `db9 before`, `db9 after`, and local `PostgreSQL 18.3`
- `Deliverables`:
- one design note with milestone ordering and capability contract
- one benchmark harness or script inventory
- baseline metrics report
- milestone-to-scenario benchmark matrix
- `Positive Acceptance`:
- all future milestones have a stable contract and measurable baseline
- `Negative Acceptance`:
- incompatible peers are rejected explicitly, not silently accepted
- missing capability negotiation blocks feature enablement
- `Exit Evidence`:
- baseline benchmark table
- metrics names and sample output
- compatibility matrix note

### CI-01-Baseline-And-Gates

- `Objective`: establish the fixed gate set that every later milestone must pass.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Inputs`: `docs/testing.md`, `docs/sot/testing-gates.md`, CSE `AGENTS.md`
- `Implementation or Test Backlog`:
- create milestone gate checklist for unit, SQL golden, PostgreSQL oracle, ORM, E2E, CSE endpoint, failpoint, and perf tests
- create a reusable pushdown compatibility matrix that covers all already-supported pushdown functions, expressions, and operators, and run it as a required regression package for every optimization milestone
- prepare reusable benchmark datasets and deterministic seeds
- prepare compatibility test jobs for `old/new`, `new/old`, and `new/new`
- define evidence format for positive and negative acceptance
- prepare local PostgreSQL 18.3 benchmark environment and reusable harness commands
- `Deliverables`:
- gate checklist document
- reproducible command list
- benchmark seed and dataset note
- PostgreSQL 18.3 comparison harness note
- pushdown compatibility matrix and command inventory
- `Positive Acceptance`:
- each later milestone can reuse the same gate package with milestone-specific additions
- `Negative Acceptance`:
- missing evidence or partial test execution blocks milestone close
- `Exit Evidence`:
- gate matrix and command inventory

## M2 Backlog

### Architect-02-Streaming-Execution

- `Objective`: remove forced `Vec<Row>` boundaries from the main scan and result path so `LIMIT` and client stop can trigger real early-stop behavior.
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`
- `Inputs`: current full materialization in `TableScanOperator::open`, `TikvStore::scan`, root executor, pgwire result encoding, CSE DB9 unary-only behavior, and the goal that early-stop must reduce payload and KV requests
- `Implementation or Test Backlog`:
- PR1: add streaming response framing and capability gating in proto
- PR2: refactor `db9-server` table scan storage path from full `Vec<Row>` return to cursor or batch stream interface
- PR3: refactor `TableScanOperator` and root executor to pull and forward rows incrementally
- PR4: refactor pgwire result encoding to support incremental row emission
- PR5: only if `db9-server`-only streaming cannot meet the milestone target, implement the smallest `cloud-storage-engine` DB9 streaming or paged response change needed for safe table-scan requests
- PR6: propagate downstream stop conditions so scan, executor, protocol, and remote cop all stop promptly when `LIMIT` is satisfied or the client closes early
- PR7: keep unsupported request shapes explicitly rejected until later milestones cover them
- `Deliverables`:
- code for streaming path
- feature flag for rollout
- metrics for first-row latency, buffered rows, rows scanned before stop, payload bytes, and KV requests
- `Positive Acceptance`:
- large table scan with `LIMIT` returns first rows without waiting for the full table
- memory growth is bounded by pipeline buffer size, not total row count
- cancel and backpressure stop work promptly
- after `LIMIT` or client early-close, upstream scan and remote streaming stop without continuing to drain the full source
- the optimized path measurably reduces payload bytes or KV requests versus `db9 before`
- `Negative Acceptance`:
- unsupported remote streaming requests still return deterministic rejection
- no silent re-materialization of full results in root executor or protocol layer
- peer without streaming capability keeps old unary path only when explicitly negotiated
- continuing to read or send rows after downstream early-stop is a failure
- `Exit Evidence`:
- before or after memory and first-row-latency comparison
- streaming smoke trace for local and remote paths
- real scenario benchmark on an `events`-like table with at least `1M` rows comparing `db9 before`, `db9 after`, and local `PostgreSQL 18.3`, including rows scanned before stop, payload bytes, and KV request count

### CI-02-Streaming-Execution

- `Objective`: validate correctness, bounded memory, compatibility, and real early-stop behavior of the streaming path.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Implementation or Test Backlog`:
- add unit tests for scan cursor lifecycle, cancellation, and early close
- add SQL tests for large scan with `LIMIT`, `OFFSET`, `ORDER BY` absent, and mixed `NULL` rows
- add PostgreSQL parity checks for visible result semantics
- add ORM tests for large result iteration and early client stop
- add CSE endpoint tests for streaming request acceptance and unsupported-shape rejection
- add failpoint tests for mid-stream cancel, timeout, and peer disconnect
- add perf gate for first-row latency and peak RSS
- add metric assertions showing that `LIMIT` or early-close reduces rows scanned, payload bytes, or KV requests versus `db9 before`
- add real scenario benchmark gate on a large append-only table with `LIMIT` and early-stop consumers, using the same harness against local PostgreSQL 18.3
- `Positive Acceptance`:
- row order and row count match current semantics
- client can stop reading early without server-side leak
- after early-stop, `db9 after` performs less unnecessary upstream work than `db9 before`
- `Negative Acceptance`:
- partial response corruption, duplicate rows, lost rows, and hanging stream fail the milestone
- old peer compatibility failures must be explicit and observable
- a benchmark where early-stop shows no reduction in scanned work requires root-cause analysis before closing the milestone
- `Exit Evidence`:
- test list and pass report
- perf table for small, medium, and large scans
- real scenario report with first-row latency, total latency, peak RSS, rows scanned before stop, payload bytes, and KV requests

## M3 Backlog

### Architect-03-Late-Materialization-And-Index-Only

- `Objective`: avoid unnecessary base-row fetch and full-row deserialization on index-driven queries, and make composite indexes serve filtering plus ordered traversal whenever possible.
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`
- `Inputs`: current path scans index keys, fetches PKs, batch-gets full rows, and deserializes whole rows; row storage is whole-row bincode; TiDB-style composite-index planning should be adapted so db9 can preserve order when equality prefixes and sort keys align
- `Implementation or Test Backlog`:
- PR1: introduce `RowLocator` or equivalent carrier plus `FetchByPkOperator`
- PR2: make planner choose `IndexProbe -> Filter/TopN/Limit -> FetchByPk` for late materialization
- PR3: add composite-index eligibility rules so planner prefers indexes that satisfy equality filters first and preserve `ORDER BY` keys next
- PR4: support narrow covering or index-only cases where projected columns already exist in index keys
- PR5: if needed, separately design `INCLUDE` or payload support in index metadata and storage encoding; keep it behind a new capability
- PR6: explicitly document which index types, predicate shapes, projection forms, and sort shapes are eligible for composite-index ordered traversal
- PR7: if covering-index payload is exposed in DDL, prefer PostgreSQL-compatible `INCLUDE (...)` syntax and document any intentional divergence explicitly
- PR8: keep the preferred implementation path inside `db9-server`; only touch `cloud-storage-engine` if planner or payload reduction goals cannot be met with local late-materialization and ordered-scan logic alone
- `Deliverables`:
- planner rule changes
- operator chain for late fetch
- index-only and composite-index eligibility documentation
- `Positive Acceptance`:
- index query projecting few columns shows sharply reduced base-row fetches and decoded rows
- eligible composite-index queries can use one index path for filter plus order, avoiding a separate sort
- `EXPLAIN` distinguishes index-only from late materialization
- `Negative Acceptance`:
- non-covering or unsafe cases do not pretend to be index-only
- cross-region or MVCC-unsafe row fetch stays rejected until safely supported
- planner must not choose a path that changes visible row semantics
- multi-value `IN`, earlier range predicates, non-prefix `ORDER BY`, mixed-direction order, or unsupported collation must not be mislabeled as order-preserving
- `Exit Evidence`:
- row-fetch-count and decoded-row reduction report
- `EXPLAIN` examples for eligible and ineligible cases
- real scenario benchmark on a `users/orders`-style table with at least `100k` rows and wide payload columns, using a composite index such as `(tenant_id, status, created_at, id)` to compare indexed lookup latency, row-fetch reduction, payload bytes, and KV requests against local `PostgreSQL 18.3`

### CI-03-Late-Materialization-And-Index-Only

- `Objective`: prove that the new access path is correct under selective, non-selective, and edge-case workloads, including composite-index filter-plus-order cases.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Implementation or Test Backlog`:
- add unit tests for planner eligibility and fetch-after-filter behavior
- add SQL tests for projection subsets, predicate selectivity, `NULL` in indexed columns, repeated keys, and composite-index prefix matching
- add negative SQL tests where multi-value `IN`, range-before-order, or mismatched `ORDER BY` must not claim ordered traversal
- add ORM tests for common lookup patterns and pagination workloads
- add CSE tests for remote rejection where row fetch is unsafe
- add perf tests showing lower fetch count and lower deserialization work
- add real scenario benchmark gate with `100k+` rows, a composite secondary index, and wide row payload to prove reduced back-to-table fetches, reduced payload bytes, reduced KV requests, and compare with local PostgreSQL 18.3
- `Positive Acceptance`:
- eligible queries avoid unnecessary base-row fetch and preserve result equality
- eligible composite-index queries avoid sort while also reducing row fetch work
- `Negative Acceptance`:
- wrong-row fetch, duplicate fetch, stale fetch, or accidental full-row materialization fail the milestone
- unsupported index shapes must be explicitly rejected or planned locally
- false positives in order-preserving detection fail the milestone
- `Exit Evidence`:
- correctness matrix by query shape
- perf comparison for index query corpus
- real scenario report with row-fetch count, decoded rows, elapsed time, p95, payload bytes, and KV requests

## M4 Backlog

### Architect-04-Ordered-Path-And-TopN

- `Objective`: turn `ORDER BY ... LIMIT` and `ORDER BY ... LIMIT ... OFFSET ...` from full sort into ordered execution and real TopN, with early-stop on ordered input.
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`
- `Inputs`: planner produces `TopNSort` but builder degrades to `Sort + Limit`; sort still materializes all rows; M3 should already provide composite-index ordered-scan eligibility that M4 must reuse
- `Implementation or Test Backlog`:
- PR1: implement real heap-based `TopNOperator`
- PR2: add order-preserving scan property for eligible B-tree scans
- PR3: propagate ordering property through planner and builder, reusing composite-index rules from `M3`
- PR4: extend operator pushdown to `Filter`, `Project`, `Limit`, and `TopN` where semantics are provably safe
- PR5: implement ordered `OFFSET + LIMIT` behavior so scanning stops after `offset + limit` qualifying index entries, and defer full-row fetch until after skip whenever possible
- PR6: enable remote ordered path only for safe request shapes and negotiated capability, and only after proving the milestone target cannot be met well enough with a `db9-server`-only ordered path
- PR7: preserve PostgreSQL ordering semantics for `ASC/DESC`, `NULLS FIRST/LAST`, collation-sensitive cases, and non-deterministic unordered `LIMIT/OFFSET` behavior
- `Deliverables`:
- TopN operator
- ordered-path planner rules
- pushdown and offset-early-stop eligibility table
- `Positive Acceptance`:
- indexed `ORDER BY ... LIMIT N` avoids full sort
- indexed `ORDER BY ... LIMIT N OFFSET M` on ordered input stops after roughly `M + N` qualifying index entries instead of reading or sorting the full result
- when late materialization is active, full-row fetches are pushed as late as possible and should trend toward `limit` rather than `offset + limit`
- `sort bytes` and peak sort memory drop materially on eligible workloads
- `Negative Acceptance`:
- unsupported collation, expression ordering, or unstable ordering cases keep local sort
- remote limit pushdown is not enabled without correct protocol and storage guarantees
- if planner cannot prove ordered traversal, it must not use early-stop claims for `OFFSET + LIMIT`
- `Exit Evidence`:
- `EXPLAIN` and metrics showing ordered path
- perf table for `ORDER BY ... LIMIT`
- real scenario benchmark on a time-ordered feed table with at least `1M` rows comparing full sort versus ordered path and local `PostgreSQL 18.3`, including `ORDER BY ... LIMIT` and `ORDER BY ... LIMIT ... OFFSET ...` cases with payload bytes and KV requests

### CI-04-Ordered-Path-And-TopN

- `Objective`: validate ordering correctness, pushdown safety, and TopN or ordered-offset performance wins.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Implementation or Test Backlog`:
- add unit tests for TopN heap logic and ordering stability
- add SQL tests for ascending, descending, ties, `NULLS FIRST/LAST`, `LIMIT`, `OFFSET`, and expression order-by in unsupported cases
- add SQL tests showing when composite indexes can preserve order and when they must not
- add PostgreSQL parity validation for visible row ordering
- add endpoint tests for remote ordered path eligibility and rejection
- add perf tests comparing full sort versus TopN
- add real scenario benchmark gate for `ORDER BY created_at DESC LIMIT N` and `ORDER BY created_at DESC LIMIT N OFFSET M` on `100k+` or `1M+` rows, and compare against local PostgreSQL 18.3
- `Positive Acceptance`:
- visible row ordering matches PostgreSQL for supported cases
- eligible queries show lower latency and memory
- eligible ordered queries also show reduced payload bytes or KV requests
- `Negative Acceptance`:
- any incorrect ordering under ties or null semantics fails the milestone
- silent pushdown on unsupported order semantics fails the milestone
- reading far beyond `offset + limit` on a provably ordered path requires explanation and blocks milestone close
- `Exit Evidence`:
- ordering correctness report
- TopN benchmark evidence
- real scenario report with sort bytes, p95 latency, first-row latency if applicable, payload bytes, and KV requests

## M5 Backlog

### Architect-05-StreamAggregate

- `Objective`: implement real stream aggregation on ordered input and remove the current fallback to hash aggregate.
- `Repos`: `db9-server`, optional `cloud-storage-engine` follow-up if aggregate protocol needs extension
- `Inputs`: planner currently picks `HashAggregate`; builder maps `StreamAggregate` back to hash aggregate
- `Implementation or Test Backlog`:
- PR1: implement `StreamAggregateOperator`
- PR2: planner chooses stream aggregate when input ordering satisfies group keys
- PR3: builder stops degrading stream aggregate to hash aggregate
- PR4: document eligibility and non-eligibility rules for aggregate shapes
- PR5: add explicit PostgreSQL compatibility notes for `GROUP BY`, `DISTINCT`, null handling, and finalization so internal stream execution cannot change visible aggregate results
- `Deliverables`:
- stream aggregate operator
- planner selection logic
- aggregate metrics for groups and buffered state
- `Positive Acceptance`:
- ordered `GROUP BY` or `DISTINCT` queries no longer need to collect all rows first
- peak memory scales with live group state instead of full input
- `Negative Acceptance`:
- unordered input must not accidentally use stream aggregate
- unsupported aggregate functions or ordering contracts must stay on hash aggregate
- `Exit Evidence`:
- memory and latency comparison against hash aggregate
- `EXPLAIN` plan samples
- real scenario benchmark on an ordered fact table with at least `1M` rows comparing hash aggregate versus stream aggregate and local `PostgreSQL 18.3`

### CI-05-StreamAggregate

- `Objective`: validate semantic equivalence and streaming-memory behavior of stream aggregation.
- `Repos`: `db9-server`
- `Implementation or Test Backlog`:
- add unit tests for group boundary transitions and final flush behavior
- add SQL tests for `GROUP BY`, `DISTINCT`, empty input, all-NULL groups, and mixed aggregate functions
- add PostgreSQL parity evidence for aggregate results
- add perf tests for ordered group-by workloads
- add real scenario benchmark gate on `100k+` or `1M+` rows for ordered `GROUP BY` and `DISTINCT`, with comparison to local PostgreSQL 18.3
- `Positive Acceptance`:
- aggregate outputs match PostgreSQL and existing correct behavior
- ordered workloads show lower peak memory than hash aggregate
- `Negative Acceptance`:
- missing final group flush, double emit, or wrong grouping under key changes fail the milestone
- `Exit Evidence`:
- aggregate correctness matrix
- memory profile comparison
- real scenario report with peak memory, elapsed time, and group throughput

## M6 Backlog

### Architect-06-Shared-Plan-Cache

- `Objective`: add broader reusable plan caching beyond the current session-local prepared cache.
- `Repos`: `db9-server`
- `Inputs`: current architecture ships session-local prepared cache only; `db9-server` is serverless-oriented so shared cache must release memory predictably; PostgreSQL compatibility should prefer generic versus custom plan controls over custom hint syntax when possible
- `Implementation or Test Backlog`:
- PR1: define shared cache key, invalidation dependencies, and tenant or search-path isolation
- PR2: introduce a global shared-plan-cache max-size parameter, tentatively named `db9.shared_plan_cache_max_bytes`, with default `1073741824` bytes (`1GB`), and implement bounded storage plus eviction behavior when the limit is reached
- PR3: integrate prepared statement and high-frequency ORM paths
- PR4: add metrics for cache hit, miss, invalidation, evictions, memory bytes, and generic versus custom plan choice
- PR5: implement PG-compatible admission and opt-out policy for parameter-sensitive SQL
- do not make SQL hints the primary control surface for skipping shared cache
- prefer PostgreSQL-compatible behavior modeled after `plan_cache_mode`: support `auto`, `force_custom_plan`, and `force_generic_plan`, and reuse PostgreSQL-compatible naming and semantics directly instead of inventing a separate hint-first API when practical
- if a DB9-specific hint is ever added later, it must be clearly documented as non-PostgreSQL compatibility sugar and not the only escape hatch
- `Deliverables`:
- shared plan-cache implementation
- invalidation documentation
- rollout flag
- shared-cache policy documentation covering max size, admission heuristics, and generic-versus-custom compatibility strategy
- `Positive Acceptance`:
- short-connection or connection-pool workloads reuse plans safely
- planning CPU and parse or analyze overhead drop on repeat workloads
- shared plan memory stays bounded by the configured global limit
- parameter-sensitive SQL can stay on custom planning without relying on custom SQL hints
- `plan_cache_mode=auto|force_custom_plan|force_generic_plan` semantics are documented and observable
- `Negative Acceptance`:
- schema drift, search path change, tenant mix, or parameter-type instability must invalidate or bypass reuse
- wrong-plan reuse or stale-plan execution fails the milestone
- unbounded shared-cache growth fails the milestone
- forcing users to adopt non-PG hint syntax as the only way to skip shared cache fails the milestone
- `Exit Evidence`:
- cache hit report
- repeated workload benchmark
- max-size eviction report
- compatibility note showing how `db9-server` maps its behavior to PostgreSQL generic/custom plan strategy
- real scenario benchmark on a short-connection ORM-style workload with at least `10k` repeated parameterized queries, including local `PostgreSQL 18.3` reference numbers

### CI-06-Shared-Plan-Cache

- `Objective`: validate plan reuse safety and real-world ORM wins.
- `Repos`: `db9-server`
- `Implementation or Test Backlog`:
- add unit tests for cache keying and invalidation
- add SQL tests for DDL invalidation, search path change, prepared statements, parameterized repeats, and max-size eviction
- add SQL tests for parameter-sensitive queries that should remain on custom planning or bypass shared generic reuse
- add ORM tests for short connections, prepared statement churn, and connection-pool reuse
- add perf tests for planning CPU and latency
- add PostgreSQL-compatibility checks for the chosen generic versus custom strategy, including `auto`, `force_custom_plan`, and `force_generic_plan`
- add real scenario benchmark gate for pooled application traffic with repeated parameterized SQL, comparing `db9 before`, `db9 after`, and local `PostgreSQL 18.3`
- `Positive Acceptance`:
- repeated eligible workloads show stable cache hits and lower planning cost
- shared-cache memory remains under the configured global limit
- parameter-sensitive queries can avoid harmful shared reuse with a PG-compatible control surface
- `Negative Acceptance`:
- stale cache entries, cross-tenant leakage, or search-path confusion fail the milestone
- max-size overflow or only-hint-based cache bypass fail the milestone
- `Exit Evidence`:
- invalidation test report
- ORM benchmark summary
- eviction evidence and parameter-sensitive query compatibility report
- real scenario report with p50, p95, planning CPU, and cache hit ratio

## M7 Backlog

### Architect-07-Parallel-And-Distributed

- `Objective`: add controlled parallel execution only after the single-threaded pipeline is clean.
- `Repos`: `db9-server`, `cloud-storage-engine`, `proto`
- `Inputs`: single-path execution should already be streaming, ordered-aware, and measurable before this milestone starts
- `Implementation or Test Backlog`:
- PR1: introduce exchange or fragment model for local parallel scan and filter
- PR2: add planner rules for when parallelism is worthwhile
- PR3: extend remote fragment execution contract in CSE if needed
- PR4: add cancellation, accounting, and skew-handling rules
- PR5: if user-visible parallel tuning controls are added, prefer PostgreSQL-compatible naming and semantics such as `max_parallel_workers_per_gather`, `parallel_setup_cost`, and `parallel_tuple_cost` where the behavior is meaningfully aligned; keep distributed-only controls under `db9.` names
- `Deliverables`:
- local or distributed parallel execution framework
- concurrency controls and metrics
- `Positive Acceptance`:
- large eligible workloads show repeatable speedup on multi-core or multi-region setups
- small queries remain on cheap single-path execution
- `Negative Acceptance`:
- skew explosion, duplicate work, unstable row ordering where order matters, or runaway task fan-out fail the milestone
- `Exit Evidence`:
- scale-up benchmark table
- task-level metrics report
- real scenario benchmark on a large analytics query over at least `1M` rows, comparing single-worker db9, parallel db9, and local `PostgreSQL 18.3`

### CI-07-Parallel-And-Distributed

- `Objective`: validate correctness under concurrency and prove that speedup is real rather than noisy.
- `Repos`: `db9-server`, `cloud-storage-engine`
- `Implementation or Test Backlog`:
- add unit and integration tests for fragment orchestration and cancellation
- add failpoint tests for timeout, worker death, region change, and partial remote failure
- add perf tests across multiple parallelism levels
- add compatibility tests for peers without distributed capability
- add real scenario benchmark gate for a large scan or aggregate workload across multiple parallelism levels, including local PostgreSQL 18.3 reference numbers
- `Positive Acceptance`:
- results stay correct under concurrency and failure injection
- eligible queries show measurable speedup with bounded overhead
- `Negative Acceptance`:
- deadlock, leaked tasks, unstable duplicate rows, or non-deterministic result loss fail the milestone
- `Exit Evidence`:
- concurrency correctness report
- parallel speedup curves
- real scenario report with scale-up efficiency, p95 latency, and resource usage

## M8 Backlog

### Architect-08-Spill-And-External-Execution

- `Objective`: make large sort, join, and aggregate workloads survive bounded memory.
- `Repos`: `db9-server`, optional `cloud-storage-engine` if remote spill coordination is introduced
- `Inputs`: current sort fully materializes in memory and hash-heavy operators do not have true external execution
- `Implementation or Test Backlog`:
- PR1: implement external sort
- PR2: implement spillable hash aggregate or join strategy
- PR3: add temp-file lifecycle, quotas, and cleanup rules
- PR4: add metrics for spill bytes, runs, passes, and cleanup failures
- PR5: prefer PostgreSQL-compatible control surfaces for analogous resource knobs, especially `work_mem`, `hash_mem_multiplier`, and `temp_file_limit`, and document any DB9-specific extensions separately
- `Deliverables`:
- spill-capable operators
- temp-space policy and rollback flag
- `Positive Acceptance`:
- large memory-bound queries complete correctly under configured limits
- spill metrics make disk usage visible
- `Negative Acceptance`:
- disk-full, temp-dir loss, cancel, and restart must fail cleanly without leak
- incorrect results caused by multi-pass merge or spilled group state fail the milestone
- `Exit Evidence`:
- bounded-memory success report
- spill failure-mode report
- real scenario benchmark on a sort, join, or aggregate workload over at least `1M` rows under tight memory limits, compared with local `PostgreSQL 18.3`

### CI-08-Spill-And-External-Execution

- `Objective`: validate correctness and robustness of spill paths under real failure modes.
- `Repos`: `db9-server`
- `Implementation or Test Backlog`:
- add unit tests for spill file format, merge passes, and cleanup
- add SQL tests for large sort, aggregate, and join under low memory thresholds
- add failpoint tests for disk-full, permission denied, partial file loss, and cancel
- add perf tests comparing in-memory and spilled execution at multiple thresholds
- add real scenario benchmark gate under constrained memory with local PostgreSQL 18.3 configured to comparable memory limits
- `Positive Acceptance`:
- bounded-memory workloads finish correctly when spill is allowed
- cleanup succeeds after normal completion and controlled failure
- `Negative Acceptance`:
- temp file leak, wrong merged order, wrong aggregate result, or hung cleanup fail the milestone
- `Exit Evidence`:
- spill correctness matrix
- temp-space cleanup report
- real scenario report with completion rate, elapsed time, spill bytes, and cleanup evidence

## Final Handoff Rule Between Milestones

- `Architect-*` closes the milestone design, implementation, metrics, and rollout note.
- `CI-*` closes correctness, compatibility, failure-mode, and performance evidence.
- The next milestone may start only after both owners mark the current milestone as passed against the common exit gate.
- If the milestone fails its gate, the team must either fix it within the same milestone or explicitly rollback behind the feature flag before moving on.

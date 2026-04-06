# Performance Optimization Design Template

**Status**: Active  
**Author**: AI Assistant  
**Date**: 2026-04-06  
**Scope**: Template for `M1` to `M7` performance optimization design docs

> **Active / non-SoT note**
>
> This document is active design-process guidance for new optimization work.
> It does not override `docs/sot/**` or current implementation truth in `src/**`.

---

## Purpose

Use this template for each optimization milestone in the `M1` to `M7` program.

Each milestone must create or update a dedicated document under `docs/design/` that explains how the optimization works.

Recommended naming:

- `docs/design/m1_streaming_execution.md`
- `docs/design/m2_late_materialization_and_composite_indexes.md`
- `docs/design/m3_ordered_path_and_topn.md`
- `docs/design/m4_stream_aggregate.md`
- `docs/design/m5_shared_plan_cache.md`
- `docs/design/m6_parallel_distributed_execution.md`
- `docs/design/m7_spill_external_execution.md`

## Required Sections

### 1. Problem

- what bottleneck exists today
- which current files or operators are involved
- why this matters for real workloads

### 2. Target Outcome

- user-visible goal
- system-level goal
- metrics expected to improve

### 3. Scope

- repo scope: `db9-server`, `cloud-storage-engine`, `proto`
- execution layers touched
- what is explicitly out of scope

### 4. How It Works

- execution flow before
- execution flow after
- planner or operator changes
- protocol or storage changes

### 5. Parameters And Rollout Controls

- new flags, GUCs, env vars, or DDL options
- scope of each parameter
- defaults
- rollout recommendation
- rollback method

If there are no new parameters, say so explicitly.

### 6. Compatibility

- PostgreSQL compatibility notes
- TiDB or TiKV non-regression notes if `cloud-storage-engine` is touched
- compatibility with all already-supported DB9 Cop pushdown functions, expressions, and operators

### 7. Early Stop, Payload, And KV Impact

For `M1` to `M3`, explicitly explain:

- whether the design enables earlier stop
- whether it reduces payload between `db9-server` and `cloud-storage-engine`
- whether it reduces KV request count
- whether it reduces full-row fetch or decode work

### 8. Index Or Ordering Rules

For milestones that touch access path or ordering:

- composite-index eligibility
- order-preserving rules
- cases that must still fall back to `Sort`
- `LIMIT` and `OFFSET` behavior

### 9. Observability

- `EXPLAIN` signals
- metrics
- logs
- debug counters

### 10. Failure Modes

- what can go wrong
- what is rejected explicitly
- what falls back
- how correctness is protected

### 11. Test Plan

- unit tests
- SQL golden tests
- PostgreSQL oracle tests
- ORM or E2E tests
- failpoint or chaos tests
- real scenario benchmark gate

### 12. Acceptance

- positive acceptance checklist
- negative acceptance checklist
- evidence required before the milestone is declared done

## Special Notes For `M5` Shared Plan Cache

If this template is used for `M5`, the design doc must explicitly cover:

- global shared cache max size and default
- eviction policy
- generic versus custom plan admission policy
- how parameter-sensitive SQL avoids harmful shared reuse
- how `plan_cache_mode=auto|force_custom_plan|force_generic_plan` maps into db9 behavior
- why the chosen escape hatch is PostgreSQL-compatible

## Special Notes For `cloud-storage-engine`

If the optimization touches `cloud-storage-engine`, the document must include a separate subsection titled `TiDB/TiKV Non-Regression Boundary` that explains why the change cannot affect existing TiDB to TiKV behavior outside the DB9-scoped path.

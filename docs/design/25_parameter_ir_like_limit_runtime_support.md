# Parameter IR Runtime Semantics for LIKE/ILIKE and LIMIT/OFFSET

## Status
- Date: 2026-02-20
- Scope: PR #888 follow-up
- Issues: #867, #868 (semantic completion), CI regressions on extended protocol

## Problem Statement
Two P0 regressions remained after the parameter-IR foundation:

1. `LIKE/ILIKE $N` fails with `42P18` (`could not determine data type of parameter`) in ORM/pg-client flows.
2. `LIMIT/OFFSET $N` fails with `Expected constant integer for LIMIT/OFFSET, got: Parameter` in optimizer build.

Both violate the single-path execution contract (`Analyzer -> Typed IR -> Optimizer -> Operators`) for prepared/extended protocol execution.

## Root Cause

### 1) LIKE/ILIKE parameter typing gap
`Analyzer::analyze_like()` analyzed child expressions but did not resolve unresolved parameter slots in LIKE context. That left `$N` unresolved at finalize-time.

### 2) LIMIT/OFFSET semantic/optimization coupling
`PhysicalNode::Limit` carries `TypedExpr` bounds, but `optimizer/build.rs` tried to evaluate them as constants during operator construction. This made constant-ness a semantic requirement, which is incorrect for parameterized execution.

## Design Goals

1. Parameter types are decided only by the Analyzer (single semantic source).
2. `LIMIT/OFFSET` supports runtime-evaluable typed expressions (including parameters).
3. Keep constant-only optimizations (`TopN`, scan limit pushdown) as optional optimizations, not semantic gates.
4. No text substitution fallback for analyzed statements.

## Architecture Decisions

### A. Analyzer owns LIKE/ILIKE param typing
In `src/sql/analyzer/expr.rs` (`analyze_like`):
- If `expr` or `pattern` is an unresolved `TypedExprKind::Parameter`, resolve that parameter slot to `DataType::Text`.
- Rebuild the parameter node with `DataType::Text` when newly resolved.

Effect:
- Parse-time analysis freezes parameter slot types correctly.
- `Describe` and `Execute` see the same type decision.

### B. LIMIT/OFFSET semantics move to runtime operator evaluation
In `src/sql/operators/limit.rs`:
- Add expression-aware constructor (`new_with_exprs`) with `Option<TypedExpr>` for `limit` and `offset`.
- Evaluate bound expressions once in `open()` using `ExecutionContext.query_ctx` and `eval_typed_expr`.
- Store resolved `usize` bounds for the streaming `next()` loop.

Effect:
- Parameterized `LIMIT/OFFSET` works in the normal typed execution path.
- No compile-time constant requirement for correctness.

### C. Optimizer build uses constants only for optimization
In `src/sql/optimizer/build.rs` for `PhysicalNode::Limit`:
- Do **not** fail when bound expressions are non-constant.
- Try constant extraction only opportunistically (`ok()` path):
  - enable scan-limit pushdown only when `LIMIT` is constant and `OFFSET` is absent/constant `0`.
- Always build `LimitOperator::new_with_exprs(...)` for semantic execution.

Effect:
- Optimization remains deterministic and safe.
- Non-constant/parameter bounds execute correctly without fallback.

## Contracts Preserved

1. Single-path execution remains intact (no alternate execution mode for parameterized limit/like).
2. Analyzer remains the only parameter type engine.
3. No runtime text substitution is introduced.
4. Constant optimizations are strictly optional and semantics-preserving.

## Test Plan

1. Analyzer regression tests:
- `LIKE $1` infers `Text`.
- `ILIKE $1` infers `Text`.

2. Optimizer/build regression tests:
- Building a `PhysicalNode::Limit` with parameter expressions succeeds (no constant-only error).

3. Runtime helper tests in `LimitOperator`:
- Parameterized bounds evaluate to non-negative integers.
- Invalid/negative values error out with clear messages.

4. Repository gates:
- `cargo fmt -- --check`
- `cargo build`
- `cargo test`

## Non-Goals

1. This change does not remove all legacy heuristic helpers unrelated to these two semantic regressions.
2. This change does not redesign `TopNSort`; it keeps current behavior and only decouples optimization from semantic correctness.

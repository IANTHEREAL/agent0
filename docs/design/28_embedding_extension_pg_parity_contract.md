# Embedding Extension Compatibility Contract

## Status
- Date: 2026-03-04
- Scope: PR #1387 review churn and follow-up contract hardening
- Decision status: Accepted architecture direction; implementation follow-up required
- Source of truth: `docs/sot/extensions-gin.md` (normative), this doc (rationale + decision record)

## Why This Exists
Repeated review cycles showed disagreement on the same two boundaries:

1. Visibility boundary: when `embedding()` / `extensions.embedding_usage()` are considered resolvable.
2. Error boundary: which SQLSTATE belongs to resolution-time failures vs runtime failures.

Without one explicit contract, local fixes regress on later rounds and review outcomes split.

## Product Strategy Alignment
This contract follows project-level strategy:

- **PG-compatible by default** for SQL behavior and SQLSTATE.
- **Intentional divergence is allowed** only when it provides clear DB9 value and is explicitly documented, tested, and tracked.

Reference: `docs/sot/README.md` section `Compatibility Strategy: PG-Compatible by Default, DB9-Better by Explicit Design`.

## Observed Behavior (Evidence Summary)

### PostgreSQL 17.7 (two-session reproductions)
- Concurrent `DROP EXTENSION` after `BEGIN` can produce `42883` on extension function calls.
- Concurrent `CREATE EXTENSION` after `BEGIN` is not reliably explained by a single pure rule in all observed scripts; outcomes can vary with session history and path.

### db9 current behavior (as of this doc date)
- Embedding visibility gate uses:
  - in-txn DDL delta override first,
  - explicit-transaction statement path bound to a transaction-consistent snapshot source,
  - autocommit statement path bound to latest committed metadata at statement boundary.
- Missing extension visibility maps to `42883` (function-not-found semantics).

## Contract Decision

### 1) Visibility model for extension-gated embedding functions
For `embedding()` and `extensions.embedding_usage()`:

- In explicit transactions:
  - apply in-transaction extension DDL delta first,
  - otherwise evaluate visibility from one transaction-consistent source for the whole statement path.
- In autocommit mode:
  - evaluate against latest committed metadata at statement boundary.
- Missing visibility MUST map to function-not-found semantics (`42883`), not `0A000`.

Design intent:
- prefer deterministic DB9 semantics and cross-surface consistency over accidental mixed timelines.
- document and test any residual PostgreSQL divergence explicitly.

### 2) Resolution boundary vs runtime boundary

Resolution-time (`42883`) MUST cover:
- missing extension function visibility,
- wrong arity,
- wrong signature/type resolution (`embedding(42)`),
- non-literal text expression in arg3 without explicit cast.

Runtime MUST cover:
- permission denied (`42501`),
- value-domain validation on valid signatures (`22023`),
- service not configured (`0A000`),
- internal faults (`XX000`).

### 3) Implicit literal cast boundary (`dimensions`)
- `embedding(text, text, '1024')` is accepted (PG-style unknown literal coercion).
- `embedding(text, text, text_column)` returns `42883` unless explicitly cast.

## SQLSTATE Matrix

| Scenario | Boundary | SQLSTATE |
|---|---|---|
| Extension function not visible | resolution | `42883` |
| Wrong arity/type signature | resolution | `42883` |
| Quoted numeric literal for arg3 (`'1024'`) | resolution | accepted |
| Non-literal text arg3 without cast | resolution | `42883` |
| Non-superuser execution | runtime gate | `42501` |
| Invalid value domain on valid signature | runtime validation | `22023` |
| Embedding service unavailable by config | runtime capability | `0A000` |
| Internal failure | runtime/internal | `XX000` |

## Verification Matrix

1. Unit/analyzer tests for signature-resolution boundary (`42883`).
2. SQL integration tests for concurrent `CREATE/DROP EXTENSION` visibility flows.
3. SQLSTATE assertions for `42883` / `42501` / `22023` / `0A000`.
4. Cross-surface consistency checks in transaction flows (catalog observation vs embedding gate path).
5. Reproduction evidence in PR: PostgreSQL version, scripts, raw outputs, execution date.

## Follow-up Implementation Work
Follow-up issues are required for implementation and governance hardening:

- Add compatibility marker enforcement (`PG_PARITY` / `DB9_DIVERGENCE(...)`) for SQL tests.
  - Tracking: https://github.com/c4pt0r/db9-server/issues/1420

## Non-goals
1. This document does not redesign provider-side embedding API behavior.
2. This document does not redesign the full extension framework lifecycle beyond embedding visibility/error contracts.

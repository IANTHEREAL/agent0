# A0 Prepared-Statement Semantic Contract (`#866`)

Status: Draft for review (to freeze before `#867` / `#868`)
Last updated: 2026-02-20
Parent roadmap: `#865`

## 1. Purpose

Define the semantic contract for PostgreSQL extended query flow in tipg:

- Parse
- Bind
- Describe (statement / portal)
- Execute

This document is a behavior contract, not an implementation plan.

## 2. Scope

In scope:

- Parameter typing rules and resolution lifecycle
- Describe metadata source-of-truth
- Execute semantics for prepared statements
- Error-surface expectations (PostgreSQL parity targets)

Out of scope:

- File/module decomposition and refactor sequencing (tracked in `#869`, `#875`)
- Optimizer/plan-cache design details (tracked in `#707`)
- Internal struct/layout choices and function names

## 3. Normative Principles

1. Single semantic source: Describe metadata and Execute semantics MUST come from the same prepared semantic object.
2. No text substitution: After Parse, parameter values MUST NOT be spliced into SQL text.
3. Stable statement identity: Parse registers one prepared statement identity; Bind/Describe/Execute reference it without changing SQL text.
4. PostgreSQL compatibility first: Behavior, coercion outcomes, and SQLSTATEs SHOULD match PostgreSQL 17.7.

### 3.1 Prepared Semantic Object (Minimum Contract)

The prepared semantic object used by both Describe and Execute MUST carry, at minimum:

1. Parameter type slots by ordinal (`$1`, `$2`, ...), including resolved types and explicit unresolved state where resolution is impossible/ambiguous under PostgreSQL rules.
2. Output column schema metadata (name + type) for statements that return rows, including DML `RETURNING`.
3. Execution semantic representation sufficient to execute without SQL re-parsing or re-analyzing (for analyzable statements).

This section defines required semantic content, not concrete Rust struct layout.

## 4. Contracted Lifecycle

### 4.1 Parse

Parse MUST:

- Accept exactly one statement for prepared execution.
- Preserve original SQL text for diagnostics/display only.
- Record client-supplied parameter OIDs from Parse.
- Build or attach semantic metadata for analyzable statements.
- Freeze prepared semantic content as immutable for the lifetime of that prepared statement identity.

Parse MUST NOT:

- Execute the statement.
- Construct substituted SQL text.

### 4.2 Bind

Bind MUST:

- Associate portal-local parameter payload bytes and format codes with an existing prepared statement.
- Keep a stable pointer to the prepared semantic object.
- Create portal binding state without mutating prepared statement semantic content.

Bind MUST NOT:

- Re-parse SQL.
- Use SQL text substitution.
- Run a separate SQL-text heuristic inference pass.
- Mutate prepared statement semantic content (including parameter/result typing metadata) after Parse.

Parameter decoding timing:

- Decode may happen at Bind or Execute as an implementation choice.
- Decode/type errors (`22P02`) MUST surface no later than the first Execute for the portal; earlier Bind-time surfacing is acceptable if behavior remains PostgreSQL-compatible.
- A NULL parameter value is valid for any resolved parameter type; downstream expression/constraint semantics then apply normally.

### 4.3 Describe Statement / Describe Portal

Describe Statement MUST:

- Return parameter metadata derived from the same semantic source used by Execute.
- Return result-column metadata derived from the same semantic source used by Execute.

Describe Portal MUST:

- Use the same result-column metadata as statement description.
- Apply portal result format codes per PostgreSQL wire protocol rules.

Describe MUST NOT:

- Build a substituted SQL string.
- Use a parallel heuristic inference engine disconnected from Analyzer/typed semantics.

### 4.4 Execute

Execute MUST:

- Execute using prepared semantic state (not a newly substituted SQL string).
- Evaluate placeholders from bound parameter values, not text literals.
- Preserve statement metadata consistency with Describe.
- Apply uniformly across supported statement classes using parameters, including `SELECT`, `INSERT`, `UPDATE`, `DELETE`, and `RETURNING` projections.

For statement classes without prepared/analyzed execution support, behavior MUST still satisfy:

- No parameter text substitution.
- PostgreSQL-compatible failure behavior for unsupported placeholder usage.

### 4.5 Statement/Portal Lifecycle and Close

Tipg MUST preserve PostgreSQL-aligned extended-protocol lifecycle semantics for named and unnamed prepared objects.

At minimum:

1. Unnamed prepared statement identity is replaceable by subsequent unnamed Parse.
2. Unnamed portal identity is replaceable by subsequent unnamed Bind.
3. Named prepared statements persist until explicit Close/DEALLOCATE or session end.
4. Portals are destroyed by Close and by normal protocol/session lifecycle boundaries.
5. Close destroys only the targeted statement or portal identity and MUST NOT mutate remaining prepared statement semantic content.

## 5. Parameter Typing Contract

### 5.1 Sources of type information

Parameter type resolution MUST be driven by:

1. Client-provided Parse OID (`OID != 0`) for `$N`
2. Semantic context (operator/function/target-column/coercion context)
3. PostgreSQL-compatible unresolved/ambiguous failure when type cannot be determined

### 5.2 Hard rules

1. Repeated `$N` occurrences share one logical type.
2. Incompatible constraints on the same `$N` MUST raise an error when no PostgreSQL-valid implicit cast/coercion path exists; no silent fallback.
3. The engine MUST NOT apply a hardcoded blanket "unknown => TEXT" fallback. Resolution to `TEXT` is allowed only when produced by normal PostgreSQL operator/function/type resolution.
4. Type compatibility and coercion outcomes MUST follow normal PostgreSQL-style operator/function resolution, not custom substitution logic.

### 5.3 Unknown OID (`OID = 0`)

`OID = 0` means "type not specified by client." The engine may infer from semantic context, including cases where PostgreSQL resolution converges to `TEXT` through normal operator/function rules. If still unresolved/ambiguous, it MUST fail with PostgreSQL-compatible error behavior (e.g. unresolved or ambiguous parameter type errors) rather than inventing a blanket default.

### 5.4 DML Coverage

Parameter typing and metadata invariants in this contract apply equally to:

- `SELECT`
- `INSERT` (including `VALUES` / `ON CONFLICT` expressions)
- `UPDATE`
- `DELETE`
- `RETURNING` output metadata

## 6. Error Contract (PG parity targets)

The following are required parity targets for extended protocol behavior.

| Scenario | Target SQLSTATE | Notes |
|---|---|---|
| Placeholder cannot be resolved to a type | `42P18` | PG 17.7 example: `SELECT pg_typeof($1)` with unknown param |
| Ambiguous operator/function due to unknown params | `42725` (operator) / PG-native function resolution code | PG 17.7 example: `unknown + unknown` |
| Placeholder used where parameters are not accepted in statement semantics | `42P02` | PG 17.7 example: `CREATE TEMP TABLE ... DEFAULT $1` |
| Parameter payload cannot decode/parse for resolved type | `22P02` | Invalid input syntax; may surface at Bind or no later than first Execute |
| Bind protocol parameter-count mismatch | `08P01` | Protocol violation target |

Important:

- Tipg MUST not remap analyzer/executor errors to unrelated SQLSTATEs at protocol boundary.
- Final SQLSTATE mapping details are validated in `#867/#868` parity tests.

## 7. Invariants

1. Describe/Execute metadata invariant:
   - For the same prepared statement/portal, Describe metadata and Execute output metadata MUST agree.
2. No-substitution invariant:
   - No SQL text rewrite/substitution occurs between Parse and Execute.
3. Single-source invariant:
   - There is exactly one semantic source for parameter/result typing in extended protocol.
4. Simple-query isolation:
   - Simple query protocol behavior remains unchanged by this contract.
   - Extended-protocol prepared/portal semantic state MUST NOT leak into or mutate simple-query semantics, beyond normal shared session state (transaction/GUC/auth context).
5. Prepared-statement immutability invariant:
   - Once Parse completes for a statement identity, its semantic content is immutable. Bind/Describe/Execute may reference it, but MUST NOT rewrite it.

## 8. PostgreSQL 17.7 Validation Notes

The following parity samples were validated against local PostgreSQL 17.7 on 2026-02-20 and are in-scope anchors for `#867/#868`:

- `unknown + unknown` ambiguity -> `42725`
- unresolved parameter type (`pg_typeof($1)` with unknown) -> `42P18`
- placeholder in `CREATE TABLE ... DEFAULT $1` -> `42P02`

These samples are contract anchors; implementation PRs MUST add reproducible parity tests in tipg.

### 8.1 Future Parity Targets (Beyond `#867/#868`)

The following PostgreSQL behavior is important but not a hard gate for `#867/#868` implementation scope:

- prepared-plan invalidation on schema/result-shape drift -> `0A000` (`cached plan must not change result type`)

This is tracked as future parity work aligned with plan-cache/runtime invalidation architecture (`#707` and related follow-ups).

## 9. Non-Goals for `#866`

This issue does not decide:

- Concrete Rust type names/fields (example: exact `PreparedStatement` struct layout)
- Exact decode stage placement (Bind vs Execute)
- File deletion sequencing (`type_infer.rs`, substitution module cleanup)

Those are implementation concerns for `#867` / `#868`, constrained by this contract.

## 10. Exit Criteria for `#866`

1. This contract is reviewed and accepted by architecture owner.
2. `#867` and `#868` explicitly reference this contract as normative baseline.
3. Any deviation in implementation PRs is documented and justified with PostgreSQL evidence.

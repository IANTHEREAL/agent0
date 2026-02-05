# GitHub Issue as SSOT — Task Process (v0)

This document defines how we run tasks **on GitHub Issues as the single source of truth (SSOT)** for task process, ownership, and verification.

Reference exemplar (multi-phase collaboration pattern): https://github.com/c4pt0r/tipg/issues/368

Inputs:
- Roles: `ROLE_DEFINITIONS.md`
- Council governance: `ARCHITECTURE_COUNCIL_CHARTER.md`

## 0) Non-negotiables

- The **GitHub issue is SSOT** for the task: goal, scope, decisions, role assignments, progress, verification evidence, and final verdict.
- System behavior/contracts **SoT is `docs/sot/**`** (authoritative spec). Implementation/review/verification MUST treat SoT as the contract, unless AC approves a contract change and SA updates SoT accordingly.
- If a requirement matters, it **MUST become a checkable constraint**: explicit DoD + verification command(s) and/or required checks.
- If plan/scope/DoD changes, we enforce reprioritization via **stop-the-line**: `state:stale-plan` + required ACK (§6).
- “Looks done” is not “done”: the Verifier MUST be able to FAIL tasks that lack required acceptance/contract/integration evidence (§7, §8).

SoT change control (hard rule):
- Any change that would alter **SoT-defined behavior/contracts/invariants** MUST be escalated via AC (`ac:needed` → decision → `ac:approved`).
- After `ac:approved`, the **SoT update is the final DoD item** and is owned by **SA**.
- Any edit to `docs/sot/**` MUST be sent for review (SA sign-off required).

## 1) Issue types (minimum set)

We use exactly these three issue types (v0):

### 1.1 Issue Packet (Validity/Triage)
Use when the primary risk is “the claim may be invalid / unreachable / mis-scoped”.

MUST include:
- Expected vs actual (contract statement)
- Relevant SoT doc(s) (if exists): `docs/sot/...`
- Minimal repro (or smallest failing CI command)
- Reachability (default vs commonly-enabled configs)
- Impact + blast radius (who/what breaks)
- Provisional severity (P0/P1/P2)

### 1.2 Stage Packet (Multi-phase execution)
Use to run complex work in phases without losing constraints.

MUST include:
- **SG** (stage goal)
- **SP** (plan: tasks/owners/deps/risks + explicit non-goals)
- **SRP** (release/rollback plan, if applicable)
- **DoD** (verifiable acceptance criteria + commands/checks)
- SoT impact: which `docs/sot/**` modules/docs are affected (or `SoT-Impact: None`)
- If SoT-defined behavior/contracts/invariants change: apply `ac:needed` and include a final DoD item “SA updates SoT (`docs/sot/**`) after `ac:approved`”.
- Links to child Execution Tasks (or a checklist if the repo prefers a single issue)

### 1.3 Execution Task (Implementable unit)
Use for concrete delivery (code/spec/tests) that a single Dev can execute.

MUST include:
- Objective (one sentence)
- Constraints (MUST/MUST NOT)
- SoT impact: which `docs/sot/**` modules/docs are affected (or `SoT-Impact: None`)
- If SoT-defined behavior/contracts/invariants change: apply `ac:needed` and include a final DoD item “SA updates SoT (`docs/sot/**`) after `ac:approved`”.
- DoD + verification command(s) / required checks
- Role assignment contract (PM/Dev/Rev/Ver mandatory; others conditional) (§3)

## 2) DoR / DoD (Definition of Ready / Done)

### 2.1 DoR (before Dev starts)
An Execution Task is DoR-ready only if the issue body has:
- Scope + explicit non-goals
- Constraints (MUST/MUST NOT)
- SoT impact is explicit: link the relevant `docs/sot/**` doc(s) or declare `SoT-Impact: None` with justification
- If SoT-defined behavior/contracts/invariants change: `ac:needed` is present and **SA is assigned** to deliver the SoT update as the final DoD item
- DoD that is independently verifiable (commands/checks + expected outputs)
- Roles assigned: **PM/Dev/Rev/Ver** MUST be named; conditional roles MUST be `N/A` with rationale or assigned (§3)
- Dependencies/risks listed (or “none known”)

If DoR is not satisfied, PM MUST block execution until fixed.

### 2.2 DoD (what “done” means)
DoD MUST be written so an independent Verifier can execute it without guesswork:
- What to run (commands / CI job / scripts)
- What output/artifact counts as PASS
- Any environment/config assumptions
- Whether the item is a **required gate** (required check) or a manual gate (explicit command + expected output)

Explicit FAIL conditions (v0):
- DoD is not verifiable (no command/check/expected result).
- DoD is verifiable but only covers a simplified subset of stated constraints.
- A required integration/contract/acceptance gate is missing or not executed when required (§7, §8).

## 3) Role assignment rules (per issue)

Role responsibilities live in `ROLE_DEFINITIONS.md`. This section only defines when roles MUST be assigned.

Mandatory (every Execution Task):
- PM, Dev, Rev, Ver

Conditional roles (assign or write `N/A` + rationale):
- SA when user-visible behavior/compatibility/contracts/invariants/cross-module boundaries are involved.
- SecA when authn/authz/security defaults/exposure surfaces/secrets/dependency risk are involved.
- SEA when repo-wide standards/gates/refactors/layering/CI policy are involved.
- DBE when schemas/critical queries/indexing/consistency/perf-critical DB semantics are involved.

## 4) Labels & states (minimal taxonomy)

Use labels to make the workflow machine-auditable.

Recommended minimal set:
- `type:*` — bug / feature / refactor / spec / gate
- `state:*` — triage / spec / ready / in-progress / review / verify / done / blocked / stale-plan
- `sev:*` — P0 / P1 / P2 (when relevant)
- `ac:*` — needed / needs-more-info / approved / rejected (council decision tracking)

## 5) Council integration (AC)

Council rules are defined in `ARCHITECTURE_COUNCIL_CHARTER.md`.

Minimum integration rules:
- Apply `ac:needed` when the charter says AC is required.
- While `ac:needed` is unresolved, stop-the-line applies: work MAY continue locally, but merges/closures MUST NOT happen based on an unapproved decision.
- AC decisions MUST be recorded via the charter’s Decision Comment template and propagated via the charter’s propagation rules.

## 6) Plan / scope / DoD changes (stop-the-line)

### 6.1 Change protocol (required)

If scope/non-goals/constraints/DoD/verification changes:
1) PM MUST update the issue body (SSOT).
2) PM MUST leave a **Decision Comment** (template below).
3) PM MUST apply `state:stale-plan` to all impacted issues/PRs until ACK is collected.
4) Dev/Rev/Ver MUST ACK before proceeding (comment `ACK: updated DoD`).
5) Reviewers MUST NOT approve merge and Verifiers MUST NOT PASS while `state:stale-plan` is present.

### 6.2 Decision Comment template (copy/paste)

```markdown
## [Decision] Scope/DoD update

### Change
...

### Why
...

### DoD / verification updates (binding)
- Added/changed:
- Removed (and why safe):

### Affected work items (must ACK and replan)
- #issue / #pr
```

## 7) Anti-simplification rules (acceptance/contract/integration)

- If integration/contract/acceptance verification is required, it MUST appear as a DoD item with:
  - required check name (preferred), OR
  - exact manual command(s) + expected output.
- SEA MUST own binding required gates to CI/required checks (or document interim manual gates with rationale).
- Ver MUST FAIL “looks done” work that lacks required acceptance evidence or shrinks DoD vs stated constraints.

## 8) Verifier reporting format (PASS/FAIL)

Verifier comments MUST be explicit and reproducible:
- Start with `VERDICT=PASS` or `VERDICT=FAIL`
- Include a DoD-by-DoD checklist result (PASS/FAIL/NOT_RUN) with evidence links or command outputs
- List commands/checks run (CI run links are acceptable)

## 9) Worked examples (v0)

### Example A — New integration acceptance gate appears late (e.g., Dify)

1) PM posts a Decision Comment adding “Dify integration check” to DoD and lists affected items.
2) PM labels affected items `state:stale-plan` until Dev/Rev/Ver ACK.
3) SEA binds the check to CI (or documents interim manual command + expected output).
4) Ver runs the check and posts `VERDICT=PASS/FAIL` with evidence.

### Example B — Oversized PR triggers AC and results in split + extra verification

1) Rev applies `ac:needed` because safe review is not feasible; a Decision Packet is posted.
2) AC decides split vs proceed and mandates extra verification/gates.
3) PM propagates DoD and labels impacted work `state:stale-plan` until ACK.
4) SEA binds gates; Ver verifies and posts `VERDICT=PASS/FAIL`.

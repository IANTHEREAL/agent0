# Role Definitions (v0)

Purpose: help agents perform as **domain experts** by making ownership, decision rights, required artifacts, and verification responsibilities explicit per task (to prevent “looks done” simplification).

## Role assignment contract (per execution issue)

Every **execution** issue MUST name:
- **PM** (owns goal/plan fidelity)
- **Dev** (implements)
- **Rev** (reviews)
- **Ver** (independent verifier; not the Dev)

Separation-of-duties rules:
- `Ver` **MUST NOT** be the same assignee as `Dev`.
- `Rev` **SHOULD NOT** be the same assignee as `Dev` (if unavoidable, record why in the issue).
- `Ver` **SHOULD NOT** be the same assignee as `Rev` (verification and review are different skills; if unavoidable, record why).

Conditional roles (write `N/A` + rationale if not needed):
- **SA** when boundaries/contracts/invariants are in play.
- **SecA** when authz/authn, security defaults, threat model, or external exposure changes.
- **SEA** when repo-wide standards, layering, CI gates, or large refactors are involved.
- **DBE** when schemas/critical queries/indexing/consistency/perf-critical DB semantics are involved.

Ownership map (who “owns” what):
- **System design:** SA
- **Security design:** SecA
- **Engineering standards & quality gates:** SEA
- **Goal alignment / plan fidelity:** PM
- **Code audit (bug finding + standards compliance):** Rev
- **Acceptance / correctness verification (independent):** Ver
- **Implementation:** Dev
- **DB correctness/perf advisory:** DBE

## Core roles

### System Architect (SA)

- **Owns:** system boundaries, invariants, external contracts, NFR budgets (reliability/perf/maintainability).
- **Required GitHub artifacts (deliverables):**
  - **Architecture spec** (issue/PR comment or docs) covering: boundaries, key flows, and what MUST NOT change.
  - **Interface contract** (API/SQL/protocol/config) including **error semantics** and **compatibility** stance.
  - **SoT ownership & updates**:
    - `docs/sot/**` is authoritative; any edits require SA authorship/sign-off.
    - If SoT-defined behavior/contracts/invariants change: SoT update happens **after `ac:approved`** and is the **final DoD item** (or explicitly declare `SoT-Impact: None` with justification).
  - **Key invariants** + **verification strategy** (contract tests / integration tests / acceptance checks).
  - **Migration/rollback plan** when behavior or persistent data semantics change.
  - **ADR/DR** when a change is cross-module, changes defaults, changes compatibility, or changes a core invariant.
- **Required engagement / sign-off when:** cross-module change; user-visible behavior change; compatibility promise changes; data semantics/invariants change; introducing a new subsystem boundary.
- **Can block:** merges/closures that violate boundaries/contracts or change behavior without a verifiable spec + verification plan.
- **Expert checklist:**
  - What contracts are changing (and for whom)?
  - What invariants are at risk? What is the blast radius if violated?
  - What is the NFR budget impact (latency/throughput/resource)?
  - What is the migration/rollback story?
  - What verification proves correctness (not just plausibility)?

### Security Architect (SecA)

- **Owns:** threat model assumptions, security requirements, abuse cases, safe defaults.
- **Required GitHub artifacts (deliverables):**
  - Threat model notes: what is trusted/untrusted, entry points, and abuse cases.
  - Security requirements + required verification (tests/checks) when risk is reachable.
  - Security decision record when changing auth boundaries, security defaults, or introducing a new exposure surface.
- **Required engagement / sign-off when:** authn/authz changes; external interfaces exposure changes; secrets/credentials handling changes; dependency risk changes; security-default changes.
- **Can block:** merges/closures that introduce reachable security risk without mitigations + verification.
- **Expert checklist:**
  - What is the attacker model and entry point?
  - Are inputs validated and outputs bounded?
  - Are secrets handled safely (logs, configs, CI artifacts)?
  - What new dependencies or permissions were added?
  - What security checks prove the mitigation works?

### Software Engineering Architect (SEA)

- **Owns:** engineering standards and **enforceable** repo-wide quality gates (structure, layering, CI policy, test strategy).
- **Required GitHub artifacts (deliverables):**
  - A **gate catalog** (what is required vs optional) for:
    - structural quality: file size, directory depth/layering rules, dependency constraints
    - code quality: lint/format, complexity caps
    - regression safety: coverage expectations (risk-based), required test suites
    - correctness gates: acceptance/contract/integration tests (as required checks when possible)
  - An **exception process**: when a gate can be skipped, who approves, and how it is recorded on GitHub (issue/PR template field + label).
  - A **binding point**: gates must map to CI / required checks (not just “recommendations”).
- **Required engagement / sign-off when:** large refactors; gate changes; repo-wide structural changes; changes that would otherwise exceed maintainability limits.
- **Can block:** work that bypasses required engineering gates or degrades maintainability without an approved exception.
- **Expert checklist:**
  - Are gates explicit, testable, and bound to CI?
  - Is there a safe exception path (and is it rare)?
  - Does the change increase coupling, file bloat, or layering violations?
  - Do tests/gates cover the real risk (not just happy-path CI)?

### Project Manager (PM)

- **Owns:** the “goal function” and plan fidelity; ensures tasks remain aligned to the North Star and current plan.
- **Required GitHub artifacts (deliverables):**
  - A **DoR-ready issue body**: scope, non-goals, constraints, DoD, verification commands, and role assignments.
  - `SoT-Impact` is explicit in execution issues/PRs:
    - link the affected `docs/sot/**` doc(s), or declare `SoT-Impact: None` with justification.
    - if SoT-defined behavior/contracts/invariants change: ensure `ac:needed` and include a final DoD item “SA updates SoT after `ac:approved`”.
  - **Decision comments** for any plan/scope/DoD change: what changed, why, and how verification changes.
  - **Anti-simplification enforcement:** if implementation only satisfies a “shrunk DoD” but misses original constraints/required integration acceptance, PM must mark the task as **NOT DONE** and either:
    - update the issue DoD (with a decision comment) and create follow-ups, or
    - escalate to SA/AC if the contract/scope is disputed.
- **Hard actions (plan fidelity):**
  - Upstream constraint/priority change ⇒ update issue SSOT (issue body + decision comment) and re-state DoD/non-goals.
- **Can block:** starting execution when the issue is not DoR-ready (unclear DoD, missing verification path, missing mandatory roles).
- **Expert checklist:**
  - Is the task executable (DoD + commands) and not “interpretation-based”?
  - Are non-goals explicit to prevent scope creep?
  - Are constraints preserved through implementation/review/verification?
  - Does “done” require independent verification evidence?

### Reviewer (Rev)

- **Owns:** PR-level **code audit**: find bugs, review correctness, and check compliance with engineering practices/standards.
- **Required GitHub artifacts (deliverables):**
  - Review verdict with evidence: what is correct, what is risky/unknown, what is missing, and requested changes.
  - If review safety is compromised (too big / too cross-cutting), a clear escalation note + rationale.
- **Can block:** merge until major issues are addressed.
- **Escalation duty:** if the PR is **too large / too cross-cutting** to be reviewed safely (broad refactor, many modules touched, unclear blast radius), Reviewer MUST flag `ac:needed` before merge.
- **Expert checklist:**
  - Does the PR map to DoD items with evidence?
  - Is `SoT-Impact` declared and correct?
  - If `docs/sot/**` is changed: is SA sign-off present, and is `ac:approved` linked when the change reflects a contract update?
  - Are tests adequate for the real risk surface?
  - Any boundary violations, hidden behavior changes, or missing error handling?
  - Is the diff reviewable? If not, should it be split or escalated?

### Issue Verifier (Ver)

- **Owns:** independent validation of the claim + acceptance; confirms reachability and runs DoD verification.
- **Required GitHub artifacts (deliverables):**
  - `VERDICT=PASS/FAIL` with exact commands/artifacts and a DoD-by-DoD checklist result.
  - Explicit FAIL if DoD is not verifiable or only covers a simplified subset of required constraints.
- **Can block:** marking “done” until verification passes.
- **Expert checklist:**
  - Can you reproduce the claim with the stated commands?
  - Are assumptions/configs stated and realistic?
  - Does verification cover integration/contract acceptance where required?
  - Does the change match `docs/sot/**`?
  - If contract changed: is there `ac:approved` + SA SoT update (final DoD) recorded with evidence?
  - Is the evidence sufficient for closure, or is it “looks ok”?

### Developer (Dev)

- **Owns:** implementation and keeping the change aligned with DoD; runs smallest relevant tests/build; provides evidence.
- **Required GitHub artifacts (deliverables):**
  - PR with clear mapping: changes → DoD items, plus test/CI evidence.
  - If SoT-defined behavior/contracts/invariants change:
    - apply `ac:needed` and provide inputs for the AC Decision Packet, and
    - provide implementation evidence for SA to update `docs/sot/**` **after `ac:approved`** (Dev MUST NOT unilaterally change SoT).
  - When required by DoD: add/modify acceptance/contract/integration tests (not just unit tests).
  - If tradeoffs are needed: document them and request the correct role’s decision (SA/SecA/SEA/DBE/AC).
- **Expert checklist:**
  - Is the solution minimal but complete (no hidden scope cuts)?
  - Are you adding the right verification, not just “more code”?
  - Did you preserve contracts/invariants and document any changes?
  - Is rollback/migration needed and documented?

## Domain advisor role

### Database Expert (DBE)

- **Owns (advisory):** DB-specific correctness/performance/consistency guidance.
- **Required GitHub artifacts (deliverables):**
  - DB impact notes (schema/query/index/transaction/consistency) + suggested verification (correctness/perf).
  - Review sign-off for DB-impacting PRs when required.
- **Required engagement / sign-off when:** schemas, critical queries, indexing, consistency semantics, or perf-critical DB paths change.
- **Expert checklist:**
  - Does this change alter query plans or indexing needs?
  - Any consistency/isolation semantics change?
  - Any risk of regressions under realistic data sizes?
  - What is the cheapest reliable perf/correctness verification?

## Council (escalation target)

### Architecture Council (AC)

AC is not an “assignee role”; it is the escalation target for decisions that exceed a single role’s authority.

- **Acts as:** approver/arbiter for cross-cutting decisions and exceptions.
- **Triggered by:** oversized/cross-cutting PRs, boundary/contract disputes, security model changes, and repo-wide gate changes.
- **Output on GitHub:** a decision comment (decision + rationale + impact + verification changes).

(Full charter is defined in `ARCHITECTURE_COUNCIL_CHARTER.md` in Phase 1.)

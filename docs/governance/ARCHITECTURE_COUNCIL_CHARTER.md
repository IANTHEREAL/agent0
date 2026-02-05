# Architecture Council (AC) Charter (v0)

This charter defines a minimal, GitHub-first council protocol that is enforceable via labels + structured comments, and is designed to prevent:
1) “Read but no reprioritization” after plan/requirement changes.
2) “Looks done but wrong” caused by shrunk DoD and missing integration/contract acceptance.

Role responsibilities are defined in `ROLE_DEFINITIONS.md` (do not duplicate them here).

## 1) Purpose (what AC exists to do)

- Guard the project’s **SSOT** for contracts, invariants, and engineering gates.
- Ensure user-facing behavior/contracts are captured and maintained in **SoT docs** (`docs/sot/**`).
- Make cross-cutting decisions **deterministic and auditable** on GitHub.
- Provide a “stop-the-line” mechanism when ambiguity or blast radius exceeds safe local decision-making.

## 2) Non-goals (what AC must NOT become)

- AC is **NOT** a court for every bug or routine PR.
- AC is **NOT** the implementer. It decides; execution remains with PM/Dev/Rev/Ver and relevant experts.
- AC is **NOT** a replacement for engineering gates; it mandates and adjusts gates, but does not manually re-run every check.

## 3) Composition (v0)

### 3.1 Core seats
AC has these core seats (humans or agents):
- **SA (Chair)** — system boundaries/contracts/invariants
- **SecA** — security model and safety
- **SEA** — engineering standards and quality gates
- **PM Lead** — goal function and plan fidelity
- **Ver Lead** — acceptance/verification feasibility and evidence quality

### 3.2 Standing advisors (as needed)
- **DBE** (and other domain experts) join when the decision touches their domain.

### 3.3 Quorum & approval rule (minimal, deterministic)

- A decision is **eligible** only if a valid **Decision Packet** exists (see §6).
- A decision is **APPROVED** when:
  - **PM Lead approves**, and
  - all **required approvers for the decision type** approve (see §4.2).
- Non-required members MAY abstain; abstention MUST NOT block within SLA (see §9).

## 4) Decision scope

### 4.1 What MUST go through AC (label `ac:needed`)

AC check-in is required for:
- **Boundary/contract/invariant changes** (user-visible behavior, compatibility, data semantics).
- **Security model changes** (threat assumptions, auth boundaries, new exposure surfaces, default security posture).
- **Repo-wide engineering gate changes** (lint/format policy, coverage thresholds, required checks, layering rules).
- **Gate exceptions** (skipping a required check) — unless an explicit, pre-approved exception policy exists.
- **Oversized / cross-cutting PRs** where safe review is not feasible (broad refactors, many modules touched, unclear blast radius).
- **SSOT insufficiency conflicts**: when two roles disagree and existing spec/gates cannot resolve it without updating SSOT.

### 4.2 Required approvers by decision type

Use the smallest required set; do not drag everyone into everything.

- **Architecture / contract / invariant:** SA (Chair) + PM Lead (+ Ver Lead if DoD/verification changes)
- **Security model:** SecA + SA (Chair) + PM Lead
- **Engineering gates / process:** SEA + PM Lead (+ Ver Lead if verification changes)
- **Acceptance/verification policy:** Ver Lead + PM Lead (+ SEA if it affects required checks)
- **DB semantics/perf-critical:** DBE + SA (Chair) + PM Lead

## 5) Escalation workflow (GitHub-first, async)

### 5.1 How to escalate

Anyone MAY raise `ac:needed`. These roles have a MUST-escalate duty:
- **Rev** MUST escalate if the PR is too large/cross-cutting to review safely.
- **PM** MUST escalate if scope/DoD changes affect multiple active tasks or invalidate existing work.
- **SA/SecA/SEA** MUST escalate if the change crosses their SSOT boundary and cannot be handled locally.

Escalation steps (minimum):
1) Apply label `ac:needed` to the tracking issue (preferred) or PR.
2) Post a **Decision Packet** comment (template in §6).
3) Tag AC members.

### 5.2 Stop-the-line rule

If `ac:needed` is present:
- Dev/Rev/Ver MAY continue **local exploration**, but MUST NOT merge/close based on an unapproved decision.
- Reviewers MUST NOT approve merge until the decision is `ac:approved` and propagated (§8).

## 6) Decision Packet (required input)

No Decision Packet ⇒ no AC decision.

Decision Packet MUST be posted as a GitHub comment using the template below.
All headings MUST be present; if a section is not applicable, write `N/A` with a short rationale.

### 6.1 Decision Packet template (copy/paste)

```markdown
## [AC] Decision Packet

**Why AC?** (why existing SSOT/gates cannot decide)

### Problem
...

### Scope / Non-goals
- In scope:
- Out of scope:

### Options considered
1) ...
2) ...

### Recommendation
...

### Impact surface / blast radius
- Affected modules/contracts:
- SoT docs impacted (`docs/sot/...`):
- Compatibility risk:
- Data semantics/invariants risk:
- Security risk:

### Migration / rollback (if applicable)
...

### ADR/DR (required?)
- Required: Yes/No
- Owner:
- Location/link (if exists):

### Verification plan (what becomes required)
- DoD changes:
- Required checks / gates:
- Acceptance / contract / integration verification:

### Affected work items (must be updated if approved)
- #issue
- #pr
```

## 7) Decision recording on GitHub

AC decisions MUST be recorded as a single, structured **Decision Comment** on the decision thread (issue or PR).

### 7.1 Decision states (labels)

- `ac:needed` — decision requested; stop-the-line applies (§5.2)
- `ac:needs-more-info` — packet incomplete; no decision yet
- `ac:approved` — decision approved; propagation required (§8)
- `ac:rejected` — decision rejected; packet may be revised and resubmitted

### 7.2 Decision Comment template (copy/paste)

```markdown
## [AC] Decision

**Status:** APPROVED / REJECTED / NEEDS_MORE_INFO
**Decision type:** (architecture / security / gates / verification / db)
**Required approvers:** (list)
**Approvals:** (link to approving comments or @mentions)
**ADR/DR:** (Required? link?)

### Decision
...

### Rationale (why this option)
...

### Constraints / invariants to preserve
- MUST:
- MUST NOT:

### Verification updates (binding)
- DoD updates:
- Required checks / gates:
- Acceptance/contract/integration requirements:
- SoT updates required (`docs/sot/...` or `SoT-Impact: None`):

### Propagation plan (who does what by when)
- PM:
- SEA:
- SA/SecA:
- Dev:
- Ver:

### Affected work items (must be updated)
- ...
```

## 8) Propagation (how decisions become binding work)

AC decisions are not “done” until propagated into the execution SSOT.

When `ac:approved`:
- **PM MUST** update the tracking issue body within SLA (§9) to reflect:
  - updated scope/non-goals/DoD/verification commands
  - role assignments (if changed)
  - link to the AC Decision Comment
- **PM MUST** mark all “Affected work items” as requiring re-check:
  - apply `state:stale-plan` to each impacted issue/PR
  - add a short comment linking the AC decision and stating “replan required”
  - require Dev/Rev/Ver ACK (`ACK: updated DoD`) before merge/closure; no ACK ⇒ blocked
- **SEA MUST** bind any gate changes to CI/required checks (or explicitly document the interim manual verification command and why CI binding is not yet possible).
- **SA/SecA MUST** update contracts/spec/invariants documentation (SoT: `docs/sot/**`) when the decision changes external behavior or security posture (or explicitly record “no SSOT impact”), and create/update ADR/DR when required by the Decision Comment.
- **Ver Lead/Ver MUST** update or create the verification task/checklist so “done” is independently verifiable.

### 8.1 Anti-“shrunk DoD” mechanism (required)

AC MUST reject (or request more info) when the Decision Packet proposes a DoD that:
- is not independently verifiable, or
- removes required integration/contract acceptance without an explicit, approved tradeoff and mitigation.

If acceptance/contract/integration verification is required, the Decision Comment MUST state:
- what exact check is required (CI required check preferred; otherwise exact command + expected output),
- who owns binding it to gates (SEA) and who owns running it (Ver/Dev).

## 9) Cadence & SLA (minimal viable)

- **Async default:** decisions are made via GitHub comments.
- **Acknowledgement SLA:** AC MUST respond with one of:
  - `ac:needs-more-info`, or
  - `ac:approved`, or
  - `ac:rejected`
  within **2 business days** of a complete Decision Packet.
- **Propagation SLA:** after `ac:approved`, PM MUST propagate (§8) within **1 business day**.

## 10) Worked examples (v0)

### Example A — Oversized refactor PR

Situation:
- Reviewer cannot safely reason about blast radius due to oversized/cross-cutting changes.

Process (minimal):
1) Rev applies `ac:needed` and posts a Decision Packet (split vs proceed + extra verification).
2) AC decides split/proceed and mandates any extra gates.
3) PM propagates DoD + labels impacted work `state:stale-plan` until ACK.
4) SEA binds gates; Ver runs acceptance and posts `VERDICT=PASS/FAIL`.

### Example B — New integration acceptance requirement appears late (e.g., Dify)

Situation:
- Mid-phase, a new requirement appears: “must pass Dify integration test”.
- Agents might acknowledge but keep executing the old DoD.

Process (minimal):
1) PM applies `ac:needed` and posts a Decision Packet (new acceptance gate; affected items listed).
2) AC approves and states the exact required verification (CI check preferred; otherwise command + expected output).
3) PM updates DoD and labels impacted work `state:stale-plan` until ACK.
4) SEA binds the gate; Ver posts `VERDICT=PASS/FAIL` with evidence.

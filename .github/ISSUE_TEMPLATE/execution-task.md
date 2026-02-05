---
name: Execution Task (Implementable Unit)
about: Concrete delivery a single Dev can execute; includes constraints, verifiable DoD, and mandatory role assignment.
title: "[Task] "
labels: ["type:task"]
---

## Objective (one sentence)

...

## Scope / Non-goals

- In scope:
- Out of scope:

## Constraints (MUST / MUST NOT)

- MUST:
- MUST NOT:

## SoT-Impact (docs/sot/**)

- SoT-Impact: (link affected `docs/sot/...`) / `SoT-Impact: None` (justification)
- SoT update owner: SA (SoT edits require SA sign-off; contract changes require `ac:approved` first and SoT update is the final DoD item)
- AC required? (Y/N). If Y: add label `ac:needed` and post an AC Decision Packet.

## DoD (verifiable)

For each DoD item, include **what to run** and **what PASS looks like**.

- [ ] DoD item:
  - Verification:
  - PASS evidence:

## Verification commands / required checks

- Commands:
- CI checks (if any):
- Environment assumptions:

## Roles (mandatory)

`Ver` MUST NOT be the same assignee as `Dev`.

- PM:
- Dev:
- Rev:
- Ver:

## Roles (conditional)

- SA: (or N/A + rationale)
- SecA: (or N/A + rationale)
- SEA: (or N/A + rationale)
- DBE: (or N/A + rationale)

## Risks / dependencies

- Risks:
- Dependencies:

## Escalation (AC)

If this task changes boundaries/contracts/invariants, security posture, or required gates, apply `ac:needed` and follow `docs/governance/ARCHITECTURE_COUNCIL_CHARTER.md`.

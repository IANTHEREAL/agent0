---
name: Stage Packet (Multi-phase Task)
about: Run complex work in phases without losing constraints; define SG/SP/SRP/DoD and link execution tasks.
title: "[Stage] "
labels: ["type:stage"]
---

## SG (Stage Goal)

...

## SP (Stage Plan)

- Tasks (with owners):
- Dependencies:
- Risks:
- Explicit non-goals:

## SRP (Stage Release / Rollback Plan)

- Release steps:
- Rollback steps:
- Rollout window (if applicable):

## DoD (Definition of Done)

Write DoD so an independent verifier can run it without guesswork.

- Acceptance criteria:
- Verification commands / required checks:
- Environment / config assumptions:

## SoT impact (docs/sot/**)

- Affected SoT modules/docs (link `docs/sot/...`), or:
- `SoT-Impact: None` (justification)
- If SoT-defined behavior/contracts/invariants change: apply `ac:needed` and include a final task “SA updates SoT after `ac:approved`”.

## Execution tasks (child links)

- [ ] #issue

## Roles (stage-level)

- PM:
- SA: (or N/A + why)
- SEA: (or N/A + why)
- SecA: (or N/A + why)
- DBE: (or N/A + why)

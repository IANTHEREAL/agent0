# governance-lint (mechanical enforcement, v0)

`governance-lint` is a fast GitHub Actions check intended to prevent obvious bypass of our GitHub-first governance protocol.

## What it enforces

The check **fails** when any of the following are true:

- The PR has label `state:stale-plan`.
- The PR has label `ac:needed`.
- The PR body does not contain an issue reference of the form `#<id>` (e.g. `Fixes #399`).

## How to fix a failure

- If you see `state:stale-plan`: update the Issue SSOT, then collect the required acknowledgements (`ACK: updated DoD`) before removing the label.
- If you see `ac:needed`: complete the AC Decision Packet + Decision (`ac:approved`) and propagate per the charter before removing the label.
- If you see “PR body must reference a tracking issue”: add an issue link in the PR body (recommended: fill `Linked issue(s):` in the PR template).

## Required-check note

To make this enforcement non-bypassable, configure branch protection to require the `governance-lint` check on the protected branches.


# governance-lint (mechanical enforcement, v1)

`governance-lint` is a fast GitHub Actions check intended to prevent obvious bypass of our GitHub-first governance protocol.

## What it enforces

The check **fails** when any of the following are true:

- The PR has label `state:stale-plan`.
- The PR has label `ac:needed`.
- The PR body does not reference at least one tracking **Issue** (not a PR) as `#<id>` (e.g. `Fixes #399`).
- The PR is **oversized** and lacks an AC-approved decision thread:
  - Oversized definition (default): `changed_files > 100` OR `(additions + deletions) > 5000`
  - Requirement: PR body must reference an **Issue** labeled `ac:approved`

## How to fix a failure

- If you see `state:stale-plan`: update the Issue SSOT, then collect the required acknowledgements (`ACK: updated DoD`) before removing the label.
- If you see `ac:needed`: complete the AC Decision Packet + Decision (`ac:approved`) and propagate per the charter before removing the label.
- If you see “PR body must reference a tracking Issue”: add an **Issue** link in the PR body (recommended: fill `Linked issue(s):` in the PR template). Referencing a PR number does not satisfy SSOT.
- If you see “Oversized PR”: split the PR, or complete AC approval and add the `ac:approved` Issue reference to the PR body.

## Configuration

The oversized thresholds are configured in `.github/workflows/governance-lint.yml` via:

- `GOV_LINT_MAX_CHANGED_FILES` (default `100`)
- `GOV_LINT_MAX_CHANGED_LINES` (default `5000`)

## Required-check note

To make this enforcement non-bypassable, configure branch protection to require the `governance-lint` check on the protected branches.

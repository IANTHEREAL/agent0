---
name: pantheon-issue-fix-loop
description: "Validate an issue with code-causal evidence, then run a strict Pantheon parallel_explore fix+review loop (codex + review_agent) until no P0/P1, then run a required local build+smoke test before merging."
---

# Pantheon Issue Fix Loop

## Overview

Follow a strict, evidence-first workflow to (1) decide whether an issue is valid and (2) if valid, iteratively fix it (codex) and review it (review_agent) until no P0/P1 remain, keeping a single PR updated.

## Inputs

- `issue_link` (required): Issue URL or identifier.
- `project_name` (required): Pantheon project name.
- `parent_branch_id` (required): Starting Pantheon branch ID (sandbox baseline).
- `fix_agent`: default to use `codex`.
- `review_agent`:default to use `review_agent` (or the configured review agent name).
- `poll_interval_seconds`: default to use `300`.

Assumption: If the user did not specify a git branch, treat branch IDs as Pantheon branches/sandboxes. Only the first Fix creates a PR; all subsequent Fix iterations push commits to the same PR head git branch.

## P0/P1 Standard (must be evidence-backed)

- **P0 (Critical/Blocker)**: Reachable under default production configuration, and causes production unavailability; severe data loss/corruption; a security vulnerability; or a primary workflow is completely blocked with no practical workaround. Must be fixed immediately.
- **P1 (High)**: Reachable in realistic production scenarios (default or commonly enabled configs), and significantly impairs core/major functionality or violates user-facing contracts relied upon (including user-visible correctness errors), or causes a severe performance regression that impacts use; a workaround may exist but is costly/risky/high-friction. Must be fixed before release.
- **Evidence bar**: A P0/P1 claim must include code-causal evidence + explicit blast-radius; borderline P1/P2 defaults to P1 unless impact is clearly narrow or edge-case only.

## Workflow (Strict)

### Step 1 — Check Issue Validity (default stance: may be invalid)

Do not propose a fix until the claim is supported by code and reachability facts.

1. Restate the issue claim precisely (expected vs actual, triggering inputs/config).
2. Locate the relevant code path(s) and identify the exact conditions required to reach them.
3. Determine reachability under **default production configuration** (or clearly-common configs).
4. Assess concrete impact and blast radius (unavailability, correctness, data safety, security, severe perf).
5. Actively search for counter-evidence (feature gates, existing guards, fallbacks, isolation boundaries, test-only behavior, unreachable branches).
6. Output a verdict:
   - `INVALID`: Provide the key code evidence/counter-evidence and stop.
   - `VALID`: Proceed to Step 2.

### Step 2 — Fix/Review Iteration Loop (Pantheon branches)

Maintain these variables throughout the loop:
- `baseline_parent_branch_id`: The initially selected Pantheon branch ID (the original baseline).
- `last_fix_branch_id`: ✅ The anchor parent for runs; initialized as `baseline_parent_branch_id`.
  - Review (and optional Verify) runs start from `last_fix_branch_id`.
  - Fix runs start from `last_fix_branch_id`, and only on successful Fix do we update `last_fix_branch_id`.
- `pr_number`, `pr_url`, `pr_head_branch`: Set during the first Fix; reused in all later Fix iterations.

Initialize at the start of Step 2:
- `baseline_parent_branch_id = parent_branch_id`
- `last_fix_branch_id = baseline_parent_branch_id`

#### 2.1 First Fix (codex) — must create PR

Call `functions.mcp__test__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
pull the latest code from master branch, then:
1) fix this issue ({issue_link}) using Linus KISS principle with an accurate, rigorous, and concise solution and don't introduce other issue and regression issue.
2) self-review your own diff (correctness, edge cases, compatibility, and obvious regressions).
3) run the smallest relevant tests/build.
4) create a PR using `gh` (MUST be created in this exploration; do NOT delegate PR creation to the user or to later steps).

Output exactly:
PR_URL=<url>
PR_NUMBER=<number>
PR_HEAD_BRANCH=<branch>
```

Wait for the branch to finish (see “Waiting / Polling”), then extract and store `PR_URL/PR_NUMBER/PR_HEAD_BRANCH`.
Set `last_fix_branch_id = fix_branch_id`.

#### 2.2 Review (review_agent) — P0/P1 bug hunt

Call `functions.mcp__test__parallel_explore` with `agent="review_agent_v1.1"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
review the code change in PR {pr_number} for issue ({issue_link}), do a bug hunt to find P0/P1 issues only.
If you find any P0/P1:
- post them to the PR as a permanent record using `gh pr comment {pr_number} --body "<your review>"`.
- then output the P0/P1 list in plain text so the orchestrator can feed it into the next Fix iteration.
Each P0/P1 must include: (1) severity P0 or P1, (2) code-causal evidence, (3) reachability statement, (4) explicit blast-radius.
Do NOT create or merge PRs in this step.
If there is no P0/P1, output exactly: NO_P0_P1
```

Wait for the branch to finish (see “Waiting / Polling”), then parse the output.
Do not update `last_fix_branch_id` in Review runs.

#### 2.3 While review reports any P0/P1

For each iteration:
1. Fix (codex): `functions.mcp__test__parallel_explore(agent="codex", parent_branch_id=last_fix_branch_id)` with prompt:

```
fix the P0/P1 issue found during coding - {p0p1_issue_descriptions} using linus KISS principle with an accurate, rigorous, and concise solution and don't introduce other issue and regression issue.

Important: do NOT create a new PR. checkout the existing PR head branch and push commits to it:
- gh pr checkout {pr_number} (or git checkout {pr_head_branch})
- commit
- push
run the smallest relevant tests/build.
```

2. Wait for the branch to finish (see “Waiting / Polling”); set `last_fix_branch_id = fix_branch_id`.
3. Review again using Step 2.2 (which uses `parent_branch_id=last_fix_branch_id`); wait and parse.

Stop the loop only when the review output is `NO_P0_P1`.

#### 2.4 Pre-merge build + smoke test (required)

Before merging, run a quick local validation on the PR head branch:
1. `cargo build --release` succeeds
2. tipg (pg-tikv) starts successfully against a local TiKV cluster
3. `pg_isready` succeeds and `SELECT 1;` works

Use the `local-tipg-up` skill for the exact commands. Run it on the PR head branch:
- `gh pr checkout {pr_number}` (or `git checkout {pr_head_branch}`)
- Follow `local-tipg-up/SKILL.md`

If this step fails, do NOT merge. Start another Fix exploration to address the failure, then rerun Step 2.2 Review (and repeat this Step 2.4 check) before merging.

#### 2.5 Merge PR

When the latest review output is `NO_P0_P1`, merge the PR using `gh` directly (this does NOT need to happen inside an exploration), or stop and report if merging is blocked by permissions/CI/review policy.

Preferred merge method: squash merge (if the repo allows it):
- `gh pr merge {pr_number} --squash` (optionally add `--delete-branch`)

If squash merge is not allowed by repo settings/policy, fall back to a normal merge:
- `gh pr merge {pr_number} --merge` (optionally add `--delete-branch`)

If the merge is blocked due to merge conflicts, start one more Fix exploration to resolve conflicts:
- Call `functions.mcp__test__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
resolve merge conflicts for PR {pr_number} (issue: {issue_link}).
Important: do NOT create a new PR. checkout the existing PR head branch and push commits to it:
- gh pr checkout {pr_number} (or git checkout {pr_head_branch})
- bring the branch up-to-date with master (rebase or merge), resolve conflicts
- run the smallest relevant tests/build
- commit and push
```

After the conflict-resolution Fix finishes, run Step 2.2 Review again (and keep the Fix/Review loop if any P0/P1 are found). Only merge after Review returns `NO_P0_P1`.

After Review returns `NO_P0_P1`, rerun Step 2.4 (pre-merge build + smoke test), then merge.

## Waiting / Polling (required between stages)

After each `parallel_explore`, wait via a sleep loop:
1. Poll `functions.mcp__test__get_branch(branch_id)` until `status` is terminal (case-insensitive match): `failed`, `succeed`, `finished`, `manifesting`, or `ready_for_manifest`.
2. Call `functions.mcp__test__branch_output(branch_id, full_output=true)` to retrieve logs/results.
3. If terminal status is `failed`, stop the workflow and report the failing `branch_id` + the relevant output snippet.
4. otherwise if it is running, sleep 300s, then poll again

Pantheon note: `manifesting` and `ready_for_manifest` mean the branch run is already done; you can fetch `branch_output` and proceed to the next step (you do not need to wait for a later `succeed`/`finished` transition).

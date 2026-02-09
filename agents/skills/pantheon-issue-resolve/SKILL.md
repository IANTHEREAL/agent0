---
name: pantheon-issue-resolve-loop
description: "Resolve an issue (create a new PR or reuse an existing PR) with evidence-first validation, then run a strict Pantheon parallel_explore fix+review+verify loop (codex) until no in-scope P0/P1 remain; finish with required local build+smoke test before merging."
---

# Pantheon Issue Resolve Loop

## Overview

Follow a strict, evidence-first workflow to (1) decide whether an issue is valid and (2) if valid, iteratively resolve it via Fix/Review/Verify (codex) until no in-scope P0/P1 remain, keeping a single PR updated (create one if needed, or reuse an existing PR).

### Golden Rule — One Fix Run at a Time

**One issue, one active Fix exploration, one PR.** Never start a second Fix exploration while the first is still running; always wait for terminal status and read `branch_output` first—impatience creates duplicate PRs.

## Inputs

- `issue_link` (Issue URL or identifier)  Or `existing_pr_link` (Existing PR URL or number).
  - Provide exactly ONE of `issue_link` or `existing_pr_link`.
  - If you start from `existing_pr_link`, derive `issue_link` from the PR (or use the PR link as the issue identifier) for the rest of this workflow.
- `project_name` (required): Pantheon project name.
- `parent_branch_id` (required): Starting Pantheon branch ID (sandbox baseline).

Assumption: If the user did not specify item as a git branch, treat branch IDs as Pantheon branches/sandboxes. If `existing_pr_link` is provided, reuse that PR (skip PR creation). Otherwise, only the first Fix creates a PR; all subsequent Fix iterations push commits to the same PR head git branch.

## P0/P1 Standard (must be evidence-backed)

- **P0 (Critical/Blocker)**: Reachable under default production configuration, and causes production unavailability; severe data loss/corruption; a security vulnerability; or a primary workflow is completely blocked with no practical workaround. Must be fixed immediately.
- **P1 (High)**: Reachable in realistic production scenarios (default or commonly enabled configs), and significantly impairs core/major functionality or violates user-facing contracts relied upon (including user-visible correctness errors), or causes a severe performance regression that impacts use; a workaround may exist but is costly/risky/high-friction. Must be fixed before release.
- **Evidence bar**: A P0/P1 claim must include code-causal evidence + explicit blast-radius; borderline P1/P2 defaults to P1 unless impact is clearly narrow or edge-case only.

## Workflow (Strict)

### Step 1 — Sync master + Check Issue Validity (codex) (default stance: may be invalid)

Before doing anything else, sync the code to the latest master, then do not propose a fix until the claim is supported by code and reachability facts.

Call `functions.mcp__pantheon__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=parent_branch_id`, and prompt:

```
pull the latest code from master branch or Existing PR: {existing_pr_link} (if having), then analyze the target deeply:
- Issue: {issue_link}
- Existing PR: {existing_pr_link} if having

0) If the issue/PR report is based on a failing test case, identify the minimal failing case from the issue/PR/CI record and rerun it on master to confirm repro (or prove non-repro) and capture the exact failure.
1) Restate the issue claim precisely (expected vs actual, triggering inputs/config).
2) Locate the relevant code path(s) and identify the exact conditions required to reach them.
3) Determine reachability under default production configuration (or clearly-common configs).
4) Identify the likely root cause and present evidence that leads to that root cause (code-causal chain, repro evidence, and why alternatives are less likely).
5) Analyze whether there is a broader systemic issue beyond this report (same pattern in adjacent code paths, shared abstractions, or config combinations).
6) Assess concrete impact and blast radius (unavailability, correctness, data safety, security, severe perf).
7) Actively search for counter-evidence (feature gates, existing guards, fallbacks, isolation boundaries, test-only behavior, unreachable branches).

Output:
- If invalid, output exactly: VERDICT=INVALID
- If valid, output exactly:
VERDICT=VALID
BEGIN_SOLUTION_SUGGESTION
<concise, actionable solution proposal using KISS; include scope, risk, and why it addresses the evidenced root cause>
END_SOLUTION_SUGGESTION
```

Wait for the branch to finish (see “Waiting / Polling”), then parse the output.
If the verdict is `VERDICT=INVALID`, stop.
If the verdict is `VERDICT=VALID`, parse `BEGIN_SOLUTION_SUGGESTION ... END_SOLUTION_SUGGESTION`, set `synced_master_branch_id = validity_branch_id`, and proceed to Step 2.

### Step 2 — Fix/Review/Verify Iteration Loop (Pantheon branches)

Maintain these variables throughout the loop:
- `baseline_parent_branch_id`: The initially selected Pantheon branch ID (the original baseline).
- `last_fix_branch_id`: ✅ The anchor parent for runs; initialized as `baseline_parent_branch_id`.
  - Review and Verify runs start from `last_fix_branch_id`.
  - Fix runs start from `last_fix_branch_id`, and only on successful Fix do we update `last_fix_branch_id`.
- `pr_number`, `pr_url`, `pr_head_branch`: Set during the first Fix; reused in all later Fix iterations.

Initialize at the start of Step 2:
- `baseline_parent_branch_id = synced_master_branch_id`
- `last_fix_branch_id = baseline_parent_branch_id`

#### 2.1 First Fix (codex) — create PR; if pr is existing, skip it

Call `functions.mcp__pantheon__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
1) fix this issue ({issue_link}) using Linus KISS principle with an accurate, rigorous, and concise solut
ion and don't introduce other issue and regression issue.
2) self-review your own diff (correctness, edge cases, compatibility, and obvious regressions).
3) run the smallest relevant tests/build.
4) create a PR using `gh` (MUST be created in this exploration; do NOT delegate PR creation to the user o
r to later steps).

Output exactly:
PR_URL=<url>
PR_NUMBER=<number>
PR_HEAD_BRANCH=<branch>
```

Wait for the branch to finish (see “Waiting / Polling”), then extract and store `PR_URL/PR_NUMBER/PR_HEAD_BRANCH`.
Set `last_fix_branch_id = fix_branch_id`.

#### 2.2 Review (codex) — P0/P1 bug hunt

Call `functions.mcp__pantheon__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
Review the code change in PR {pr_number} for issue ({issue_link}); do a P0/P1-only bug hunt.
Principle: treat the review like a scientific investigation—read as much as needed, explain what the code does (don’t guess), and only accept a P0/P1 when code evidence + reachability justify it.
Extra: if the issue/PR report is based on a failing test case (CI), rerun the minimal failing case/command (from the issue/PR/CI record) on the current PR head before concluding.
Do NOT post comments and do NOT create issues in this step.
If you find any P0/P1:
- output exactly:
P0_P1_FINDINGS
BEGIN_P0_P1_FINDINGS
<P0/P1 list>
END_P0_P1_FINDINGS
Each P0/P1 must include: (1) severity P0 or P1, (2) code-causal evidence, (3) reachability statement, (4) explicit blast-radius.
Do NOT create or merge PRs in this step.
If there is no P0/P1, output exactly: NO_P0_P1
```

Wait for the branch to finish (see “Waiting / Polling”), then parse the output.
Do not update `last_fix_branch_id` in Review runs.
If the Review output is `NO_P0_P1`, skip Verify and proceed to Step 2.5.

#### 2.3 Verify (codex) — validate + scope review findings

Goal: ensure Fix only works on *valid* issues, and decide whether each valid issue must be fixed inside this PR or can be deferred into a separate GitHub issue.

Call `functions.mcp__pantheon__parallel_explore` with `agent="codex"`, `num_branches=1`, `parent_branch_id=last_fix_branch_id`, and prompt:

```
verify the P0/P1 findings from the latest review for PR {pr_number} (issue: {issue_link}).

Inputs:
- Review findings: {p0p1_issue_descriptions} (the content between `BEGIN_P0_P1_FINDINGS` and `END_P0_P1_FINDINGS` from Step 2.2 output)

Principle: Your default stance is: each issue may be a misread, a misunderstanding, or an edge case--unless the code evidence forces you to accept it. Read as much as needed, and treat code/issue analysis like a scientific experiment—explain what the code actually does (don’t guess), challenge assumptions, and explicitly confront any gaps in understanding.


For EACH finding, do triage:
1) Validity: confirm it is real on the current PR head (or explain why it is invalid / already fixed).
2) Origin: best-effort decide whether it is introduced by this PR vs pre-existing on master.
3) Difficulty: estimate fix difficulty (S/M/L) and risk (low/med/high).
4) Scope decision (choose exactly ONE):
   - FIX_IN_THIS_PR: valid and should block merge (e.g. introduced by PR, or merging makes things worse, or must-fix P0/P1).
   - DEFER_CREATE_ISSUE: valid but does NOT need to be fixed in this PR (e.g. not introduced by PR and merge doesn't worsen, or fix is large/risky and better separated).
   - INVALID_OR_ALREADY_FIXED: not valid, duplicate, not reachable, not actually P0/P1, or already fixed by current head.

For every DEFER_CREATE_ISSUE item:
- create a GitHub issue in the same repo as the PR (avoid duplicates by searching first).
  - using `gh`:
    - `REPO=$(gh pr view {pr_number} --json baseRepository --jq .baseRepository.nameWithOwner)`
    - `gh issue list -R "$REPO" --search "<keywords> in:title,body state:open" --limit 10`
- if a matching open issue already exists, do NOT create a new one; reuse the existing issue link (optionally add a short comment with new evidence + link back to PR #{pr_number}).
- include a link back to PR #{pr_number} and include code-causal evidence + repro/impact.

Post ONE PR issue comment summarizing this triage (idempotent per PR head SHA):
- compute PR head SHA: `HEAD_SHA=$(gh pr view {pr_number} --json headRefOid --jq .headRefOid)`
- if there is already an issue comment containing `<!-- pantheon-verify:{HEAD_SHA} -->`, do NOT post again.
- post via stdin (shell-safe, preserves backticks):
  - `gh pr comment {pr_number} --body-file - <<'EOF'`
  - first line MUST be: `<!-- pantheon-verify:{HEAD_SHA} -->`
  - include THREE sections so it is unambiguous what must be fixed in this PR vs not:
    - FIX_IN_THIS_PR: each item includes severity + brief rationale + difficulty/risk.
    - DEFER_CREATE_ISSUE: each item includes the created/existing issue link + brief rationale.
    - INVALID_OR_ALREADY_FIXED: brief rationale.
  - `EOF`

Output:
- If there is NO item marked FIX_IN_THIS_PR, output exactly: NO_IN_SCOPE_P0_P1
- Otherwise output exactly:
IN_SCOPE_P0_P1
BEGIN_IN_SCOPE_P0_P1
<the in-scope P0/P1 list to feed into the next Fix step as {in_scope_p0p1_issue_descriptions}>
END_IN_SCOPE_P0_P1
```

Wait for the branch to finish (see “Waiting / Polling”), then parse the output.
Do not update `last_fix_branch_id` in Verify runs.
If the Verify output is `NO_IN_SCOPE_P0_P1`, skip Fix iterations and proceed to Step 2.5.

#### 2.4 While verify reports any in-scope P0/P1

For each iteration:
1. Fix (codex): `functions.mcp__pantheon__parallel_explore(agent="codex", parent_branch_id=last_fix_branch_id, num_branches=1)` with prompt:

```
fix the verified in-scope P0/P1 issue(s) - {in_scope_p0p1_issue_descriptions} (the content between `BEGIN_IN_SCOPE_P0_P1` and `END_IN_SCOPE_P0_P1` from Step 2.3 output) using linus KISS principle with an accurate, rigorous, and concise solution and don't introduce other issue and regression issue.

Important: do NOT create a new PR. checkout the existing PR head branch and push commits to it:
- gh pr checkout {pr_number} (or git checkout {pr_head_branch})
- commit
- push
run the smallest relevant tests/build.
```

2. Wait for the branch to finish (see “Waiting / Polling”); set `last_fix_branch_id = fix_branch_id`.
3. Review again using Step 2.2 (which uses `parent_branch_id=last_fix_branch_id`); wait and parse.
4. If Review output is `NO_P0_P1`, stop the loop.
5. Otherwise Verify again using Step 2.3; wait and parse.

Stop the loop when either:
- Review outputs `NO_P0_P1`, or
- Verify outputs `NO_IN_SCOPE_P0_P1` (i.e., remaining findings were invalid or deferred into separate GitHub issues).

#### 2.5 Pre-merge build + smoke test (required)

Before return, run a quick local validation on the PR head branch:
1. `cargo build --release` succeeds
2. tipg (pg-tikv) starts successfully against a local TiKV cluster
3. `pg_isready` succeeds and `SELECT 1;` works
4. CI required checks are green for the PR head

Use the `local-tipg-up` skill for the exact commands. It starts a local TiKV cluster (via `scripts/tikv_admin.py` / tiup), builds `pg-tikv` in release mode, starts the server, and runs a smoke test (`pg_isready` + `SELECT 1`). Run it on the PR head branch:
- `gh pr checkout {pr_number}` (or `git checkout {pr_head_branch}`)
- Follow `local-tipg-up/SKILL.md`

Then ensure CI is green (required). CI failures are merge blockers (treat as `MERGE_BLOCKER=CI_FAILED`, not a P0/P1 review finding):
- Wait for required checks: `gh pr checks {pr_number} --required --watch --fail-fast`
- If any required check fails, do NOT merge. Inspect the failure output and use it to drive the next Fix.
  - List checks: `gh pr checks {pr_number} --required`
  - If it is a GitHub Actions failure: `gh run list --branch {pr_head_branch} --limit 20` then `gh run view <run-id> --log-failed`
- If CI is red due to flaky/infra (best-effort judged as not introduced by this PR), create or reuse a GitHub issue to track it (use the same dedupe workflow as Step 2.3 `DEFER_CREATE_ISSUE`), then stop; do NOT merge until required checks are green.

If this step fails, do NOT merge. Start another Fix exploration to address the failure, then rerun Step 2.2 Review (and Step 2.3 Verify if needed), and repeat this Step 2.5 check before merging.


## Waiting / Polling (required between stages)

After each `parallel_explore`, wait via a sleep loop:
1. Poll `functions.mcp__pantheon__get_branch(branch_id)` until `status` is terminal (case-insensitive match): `failed`, `succeed`, `finished`, `manifesting`, or `ready_for_manifest`.
2. If `status` is terminal, Call `functions.mcp__pantheon__branch_output(branch_id, full_output=true)` to retrieve logs/results. 
3. Otherwise (not terminal), sleep 600s, then poll again

Pantheon note: `manifesting` and `ready_for_manifest` mean the branch run is already done; you can fetch `branch_output` and proceed to the next step (you do not need to wait for a later `succeed`/`finished` transition).

Hard rule:
- `functions.mcp__pantheon__parallel_explore` must be with `num_branches=1`
- The pantheon branch is a long running task, it will take  1h - 2h, and won't stuck. Plese Don’t treat it as a short-term task, and don’t try to start a new exploration or experiment with new approaches—this will just make things messy.

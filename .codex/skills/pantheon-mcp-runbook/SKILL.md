---
name: pantheon-mcp-runbook
description: "Pantheon MCP runbook with minimal tools: use only parallel_explore (num_branches=1) + get_branch + branch_output; run optional verify from baseline, then keep a Fix↔Review parent_branch_id lineage with sleep+polling."
---

# Pantheon MCP Runbook (Minimal Tools, `num_branches=1`)

This skill is a **step-by-step, no-ambiguity guide** for operating Pantheon via MCP using only:

- `parallel_explore`
- `get_branch`
- `branch_output`

We intentionally do not use other Pantheon MCP tools in this workflow. If required inputs (like `project_name` or `parent_branch_id`) are missing, ask the user for them instead of trying to discover them with other tools.

Examples of tools we deliberately do **not** use here: manual branch create/delete, PR linking, KV storage, background task management, etc.

Keep a minimal “run context” in the conversation so you do not lose track:

- `project_name`
- `baseline_parent_branch_id`
- `chain_parent_branch_id` (the current parent for the Fix ↔ Review chain)
- `poll_interval_seconds` (typically 300; 60 when debugging)
- For each run: `branch_id` + purpose (`verify` / `fix` / `review`)

## Mental model: remote sandbox from a parent snapshot

Each `parallel_explore(parent_branch_id=...)` creates a new Pantheon branch that acts like a **remote sandbox**:

1. A new branch is created from the **parent snapshot** (`parent_branch_id`).
2. The sandbox inherits the parent’s environment/data snapshot.
3. The selected `agent` runs inside that sandbox.

You then poll the resulting `branch_id` until completion and read results from `branch_output`.

## Tool reference (exactly what we use)

### `parallel_explore`

What it does:

- Creates a new Pantheon branch/sandbox derived from `parent_branch_id`.
- Runs the selected `agent` inside that sandbox.
- Returns a `branch_id` you will poll.

How we call it (best practice):

- Always set `num_branches=1`.
- Always pass a **list** to `shared_prompt_sequence` (even if it has only one prompt).

### `get_branch`

What it does:

- Reads the current status/metadata of a `branch_id`.
- Used only for polling and determining when to fetch final logs.

### `branch_output`

What it does:

- Fetches the branch logs/results for a `branch_id`.

How we call it (best practice):

- Use `full_output=true` when you need complete logs (especially on failures).
- Prefer fetching after `get_branch` reports a terminal status.

## Agents (recommended roles)

- `codex`: **general-purpose** (develop, fix, refactor, run tests; can also do ad-hoc verify/review if needed).
- `verify_agent`: optimized for **issue validation** (repro, reachability, evidence-backed verdict).
- `review_code`: optimized for **code review** (bug/risk finding, actionable feedback).

## Best practice: `num_branches=1`

Standardize on `num_branches=1` for determinism (single sandbox + single log stream) and easier debugging.

Why we standardize on `num_branches=1`:

- Deterministic orchestration: one sandbox, one log stream.
- Easier diagnosis: fewer moving parts when a run fails.
- Lower operational noise/cost.

If you later need parallelism, increase `num_branches` intentionally and label outputs per-branch; this runbook assumes `num_branches=1`.

## Required inputs (ask if missing)

To run this workflow without ambiguity, you need:

- `project_name`
- `baseline_parent_branch_id` (the starting snapshot)
- A prompt with a clear objective + necessary context

If any of these are not provided, stop and ask the user.

## Prompt guidance (minimal requirement)

Each prompt must include:

- A clear objective (what “done” means)
- Necessary context (expected vs actual behavior, repro steps/input, constraints, and any commands/tests you want run)

## Polling pattern (sleep + poll)

`parallel_explore` can run for a long time; always poll with `get_branch` until terminal, then fetch logs:

```text
branch_id = parallel_explore(...).branch_id

while true:
  info = get_branch(branch_id)
  if info.status in {"succeed","failed","finished"} (case-insensitive):
    out = branch_output(branch_id, full_output=true)
    break
  sleep(poll_interval_seconds)  # typically 60–300 seconds
```

Do not try to interpret success/failure from partial logs while the branch is still running; wait for a terminal status first.

Treat `failed` as failure; treat `succeed` / `finished` as completion (still read `branch_output` to confirm outcome).

When polling, actually sleep between checks (e.g., run a local `sleep 300`) so you do not spam `get_branch`.

Polling operational notes:

- Prefer `poll_interval_seconds=300` for long runs; use shorter intervals (e.g. 60s) only when debugging.
- If logs look truncated or incomplete, re-fetch with `branch_output(full_output=true)`.
- On failures, always capture and report: `branch_id` + the relevant failure snippet from `branch_output`.

## Workflow (recommended): Fix ↔ Review chain (with lineage)

The primary best practice is a **Fix → Review → Fix → Review** lineage so the reviewer always sees the latest fixed code, and each fix builds on the reviewed snapshot:

```text
baseline_parent -> fix_1 -> review_1 -> fix_2 -> review_2 -> ...
```

Agent mapping for this chain:

- Fix runs: `agent="codex"`
- Review runs: `agent="review_code"`

### Optional: Verify before fixing

If you need to validate an issue first, run a **separate** verify step:

- `parallel_explore(..., parent_branch_id=baseline_parent_branch_id, agent="verify_agent")`
- Poll and read `branch_output`
- If the issue is invalid/unreproducible, stop.
- Otherwise proceed to the Fix ↔ Review chain **starting from `baseline_parent_branch_id`**.

Keeping verify separate avoids accidentally “threading” any verify-side artifacts into the fix lineage.

## Minimal run templates (no output format)

All runs follow the same mechanics:

1. Start: `parallel_explore(project_name=..., parent_branch_id=..., num_branches=1, agent=..., shared_prompt_sequence=[prompt])`
2. Poll: `get_branch(branch_id)` in a sleep loop until terminal
3. Read logs: `branch_output(branch_id, full_output=true)`

Templates:

- Verify: `parent_branch_id=baseline_parent_branch_id`, `agent="verify_agent"`
- Fix / Develop: `parent_branch_id=chain_parent_branch_id`, `agent="codex"`
- Review: `parent_branch_id=chain_parent_branch_id`, `agent="review_code"`

## No-ambiguity chaining rules (how to set `parent_branch_id`)

Maintain a single variable for the Fix ↔ Review lineage: `chain_parent_branch_id`.

Initialize:

- `chain_parent_branch_id = baseline_parent_branch_id`

Then for each run:

1. Start a run with:
   - `num_branches=1`
   - `parent_branch_id=chain_parent_branch_id`
   - `shared_prompt_sequence=[prompt]` (a 1-element list; do not pass a bare string)
2. Poll with `get_branch(branch_id)` until terminal.
3. Fetch logs with `branch_output(branch_id, full_output=true)`.
4. If the branch status is `failed`, do **not** update `chain_parent_branch_id` (rerun from the last known-good parent).
5. If the branch status is `succeed` or `finished`, update:
   - `chain_parent_branch_id = branch_id`

This rule eliminates confusion about where the next run should start.

Practical effect:

- After a successful Fix, the next Review will run with `parent_branch_id = fix_branch_id`, so the reviewer sees the fixed code.
- If Review reports issues, the next Fix runs with `parent_branch_id = review_branch_id`, creating a clean linear lineage across iterations.

## Failure handling

If a branch fails:

1. Pull full logs with `branch_output(full_output=true)` and record the failing `branch_id`.
2. Sanity-check the run actually attempted what you asked (prompt ambiguity is common).
3. Classify the failure: missing context, prompt ambiguity, infra, test flake, or a real bug.
4. Rerun from the last known-good `chain_parent_branch_id` with a clarified prompt (more specific objective + necessary context/repro).

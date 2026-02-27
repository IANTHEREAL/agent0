# tipg-test-autopilot

## Responsibility

Automatically select test lanes based on changes, run tests, collect coverage, and generate gap/backlog artifacts.

## Trigger Conditions

1. A feature or fix implementation is completed and enters testing.
2. There is a valid `base_sha..head_sha` diff.

## Inputs

1. `base_sha`
2. `head_sha`
3. `mode` (`pr` / `main_push` / `cadence` / `nightly`)
4. `DSN` (optional, local default allowed)
5. Rule source: `auto_testing/coverage_map.yaml`

## Execution Steps

1. Run: `bash scripts/test_orchestrator.sh <base_sha> <head_sha> <mode>`
2. Execute selected lanes (from map + changed files).
3. Collect coverage: line/statement/scenario/area/path.
4. Generate gaps: `artifacts/coverage/gap_list.json`.
5. Generate tasks: `artifacts/agent_backlog.json`.

## Output Artifacts

1. `artifacts/test_report.json`
2. `artifacts/test_report.md`
3. `artifacts/coverage/*.json`
4. `artifacts/agent_backlog.json`
5. `artifacts/logs/<lane>.log`

## Failure Handling

1. Lane failure: stop/continue by `fail_fast` policy.
2. Coverage below threshold: set `merge_allowed=false`.
3. Preserve minimal rerun commands in `rerun_commands`.

## Exit Criteria

1. Reports and coverage artifacts are persisted.
2. `merge_allowed` is explicitly decided.

# tipg-pr-readiness-gate

## Responsibility

Aggregate test and coverage evidence and emit the final `merge_allowed` decision.

## Trigger Conditions

1. `tipg-test-autopilot` is complete.
2. `tipg-failure-triage` output is available (if failures exist).

## Inputs

1. `artifacts/test_report.json`
2. `artifacts/agent_backlog.json`
3. `artifacts/coverage/*.json`
4. Optional: `artifacts/triage/failure_triage.json`

## Execution Steps

1. Verify required lanes for current mode were executed.
2. Verify thresholds for line/statement/scenario coverage.
3. Verify there are no uncovered critical paths.
4. Verify no unresolved P0 tasks remain in backlog.
5. Emit merge decision with blocking reasons.

## Output Artifacts

1. `artifacts/gate/pr_readiness_gate.json`
2. `artifacts/gate/pr_readiness_gate.md`

## Failure Handling

1. Any hard gate fails: `merge_allowed=false`.
2. Missing evidence: set status to `blocked_by_missing_artifact`.

## Exit Criteria

1. Explicit `merge_allowed=true|false` is produced.
2. If `false`, blocking items and minimal next actions are listed.

# tipg-failure-triage

## Responsibility

Classify failures by root layer and provide minimal repro plus next repair actions.

## Trigger Conditions

1. Any lane fails.
2. Coverage gate fails (large gaps or missing critical paths).

## Inputs

1. `artifacts/test_report.json`
2. `artifacts/logs/*.log`
3. `artifacts/coverage/gap_list.json`
4. `artifacts/coverage/critical_path_coverage.json`
5. `changed_files`

## Execution Steps

1. Read failed lanes and exit codes.
2. Classify by root layer: `Analyzer` / `Executor` / `Catalog` / `Protocol` / `Storage` / `Test` / `Env`.
3. Extract minimal repro commands.
4. Assign priority and owner skill for each failure item.

## Output Artifacts

1. `artifacts/triage/failure_triage.json`
2. `artifacts/triage/failure_triage.md`

## Failure Handling

1. Missing logs/evidence: mark `need_more_evidence` and request targeted rerun.
2. Multi-layer failures: resolve lowest blocking layer first (`Env/Protocol/Storage`).

## Exit Criteria

1. Every failure item has a root-cause layer.
2. Every failure item has an executable next action.

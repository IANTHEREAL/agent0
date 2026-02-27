# tipg-pg-oracle-check

## Responsibility

Validate SQL test expectations against PostgreSQL behavior to prevent incorrect expected files from masking engine issues.

## Trigger Conditions

1. Any change in `tests/*.sql`, `*.expected`, `*.errors`, or `*.assert`.
2. Any SQL semantic/type/error behavior change.

## Inputs

1. Changed test case paths.
2. PostgreSQL DSN.
3. tipg DSN.

## Execution Steps

1. Execute target `.sql` on PostgreSQL and capture output.
2. Execute the same `.sql` on tipg and capture output.
3. Compare PostgreSQL output with expected files.
4. Update expected files only after PostgreSQL parity is confirmed.

## Output Artifacts

1. `artifacts/oracle/oracle_diff.json`
2. `artifacts/oracle/oracle_diff.md`
3. `artifacts/oracle/<case>.pg.out`
4. `artifacts/oracle/<case>.tipg.out`

## Failure Handling

1. PostgreSQL vs expected mismatch: block and fix expectations first.
2. PostgreSQL matches but tipg mismatches: delegate to `tipg-failure-triage`.

## Exit Criteria

1. All changed cases are validated against PostgreSQL.
2. Every diff has explicit ownership (expectation issue vs engine issue).

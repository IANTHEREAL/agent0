# pgx driver lane (lane A-DRIVERS)

Runs **pgx's own test suite** (Go's PostgreSQL driver) against db9 — the
connect-path surface (extended protocol, binary type/OID codecs, COPY,
pipelining) from a different implementation than psycopg. Failures are the gap
registry — see `pgx-bank.md`.

## Run it
```bash
# needs a running db9 (see the local-db9-standup recipe) + go/git/psql
bash auto_testing/corpora/drivers/pgx/run.sh           # full suite
bash auto_testing/corpora/drivers/pgx/run.sh ./pgtype  # one package

# or via the e2e entrypoint (db9 location from PG_DSN):
PG_DSN=postgres://admin:admin@127.0.0.1:5455/postgres bash scripts/e2e_tests.sh pgx
```
First run clones pgx (pinned `v5.10.0`, into `$PGX_DIR=/tmp/pgx`); points it at db9
via `PGX_TEST_DATABASE`. `go test` isolates per package (each package is its own
test binary), so one package's crash can't lose the rest.

## Cross-driver signal
pgx confirms the same dominant db9 gaps psycopg found — **extended-protocol `$N`
parameter type inference**, `inet` type, `statement_timeout` default, `transaction_read_only`
GUC — from an independent Go implementation, plus new ones (`generate_series` in
non-SRF context, range-type DDL/constructors, array binary codec). The bulk of
`relation already exists` failures are test-isolation (pgx reuses fixed table names),
not independent db9 bugs — flagged as such in the bank.

## Findings model
`pgx-bank.md` = every failing test verbatim (package::Test + exact db9 error),
grouped by root cause. Query semantics never masked.

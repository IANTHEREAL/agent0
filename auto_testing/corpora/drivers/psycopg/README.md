# psycopg driver lane (lane A-DRIVERS)

Runs **psycopg3's own test suite** against db9 — the **connect-path** surface
(extended-protocol parameter type inference, binary type/OID codecs, SQLSTATE,
COPY, transaction control, catalog introspection) that the pg_regress (pure SQL)
and Django (ORM) lanes don't reach. The failures are the gap registry — see
`psycopg-bank.md`.

## Run it
```bash
# needs a running db9 (see the local-db9-standup recipe) + python3/git/psql
bash auto_testing/corpora/drivers/psycopg/run.sh           # smoke (core DB-behavior files)
bash auto_testing/corpora/drivers/psycopg/run.sh full      # all tests/test_*.py
bash auto_testing/corpora/drivers/psycopg/run.sh tests/test_connection.py

# or via the e2e entrypoint (db9 location from PG_DSN):
PG_DSN=postgres://admin:admin@127.0.0.1:5455/postgres bash scripts/e2e_tests.sh psycopg
```
First run clones psycopg (pinned `3.3.4`, into `$PSYCOPG_DIR=/tmp/psycopg`) and builds
a venv (`$VENV=/tmp/pgvenv`); later runs reuse them.

## Design notes
- **Non-editable install** of psycopg (an `-e` install leaves a namespace finder the
  bare project dir shadows → broken `import psycopg`).
- **Per-file isolation + `--cache-clear`**: psycopg's conftest caches a "segfault"
  flag and refuses to run after any crash; per-file runs + clearing the cache stop
  one file's crash/hang (itself a db9 finding) from blocking the rest.
- **Tooling excluded**: `test_typing.py` (mypy), `subprocess`/`slow`/`flakey`/`timing`
  markers — not db9 gaps.
- **CRDB cross-reference**: psycopg ships `@crdb_skip` (tests CockroachDB — also a
  distributed PG engine — skips). A db9 failure that is *also* crdb_skip is an
  expected distributed-engine divergence; one that is **not** is db9-specific and
  prioritized. `classify_failures.py` tags every failure `[db9]` or `[dist]`.

## Findings model
- `psycopg-bank.md` = every failing case verbatim (nodeid + exact db9 error),
  grouped by root cause, db9-specific vs distributed-divergence. Query semantics
  are never masked.

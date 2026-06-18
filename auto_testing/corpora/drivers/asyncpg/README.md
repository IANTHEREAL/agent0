# asyncpg driver lane (lane A-DRIVERS)

Runs **asyncpg's own test suite** against db9 — the **connect-path** surface
(extended protocol, **binary** type/OID codecs, prepared statements, cursors, COPY,
catalog introspection) from a **third independent implementation** (Python async)
after psycopg (Python sync) and pgx (Go). asyncpg is binary-protocol and
type-introspection heavy, so it stresses codecs harder than psycopg. The failures
are the gap registry — see `asyncpg-bank.md`.

## Run it
```bash
# needs a running db9 (see the local-db9-standup recipe) + python3 + python3-dev + git + psql + a C compiler
bash auto_testing/corpora/drivers/asyncpg/run.sh                 # smoke (core DB-behavior files)
bash auto_testing/corpora/drivers/asyncpg/run.sh full            # all tests/test_*.py (slow)
bash auto_testing/corpora/drivers/asyncpg/run.sh tests/test_codecs.py

# or via the e2e entrypoint (db9 location from PG_DSN):
PG_DSN=postgres://admin:admin@127.0.0.1:5455/postgres bash scripts/e2e_tests.sh asyncpg
```
First run clones asyncpg (pinned `v0.30.0`, into `$APG_DIR=/tmp/asyncpg`, with
submodules), builds its C extensions in-place, and creates a venv
(`$APG_VENV=/tmp/apgvenv`); later runs reuse them.

## Design notes
- **#2721 close-hang workaround.** asyncpg's graceful `Connection.close()` **hangs**
  against db9 — db9 doesn't close the socket on the `Terminate` (`X`) message
  (verified: `close()` >8s vs `terminate()` 0.00s). Every test's teardown calls
  `close()`, so the suite can't complete unmodified. `run.sh` writes a `conftest.py`
  that makes `close()` abrupt (`terminate()`). This is itself a logged db9 gap
  (**#2721**); the workaround does **not** mask query semantics, only the
  connection-teardown path.
- **Build needs the pgproto submodule** (`--recurse-submodules`), `setuptools<81`
  (`pkg_resources` removed in newer setuptools / py3.12), `python3-dev` (`Python.h`),
  `Cython`, and an in-place `build_ext` — asyncpg ships `.pyx` compiled against
  pgproto.
- **Per-file pytest runs + `pytest-timeout`**: each file isolated; a per-test
  timeout catches any residual op-level hang (rather than stalling the whole run).
  Results captured via `pytest-json-report` for machine-readable classification.
- **`already exists` failures are mostly test-isolation artifacts** — asyncpg reuses
  fixed table/type names and the close→terminate workaround skips graceful cleanup,
  so residue collides on later tests. `classify_failures.py` buckets these as
  `test-harness`, not db9 bugs.

## Findings model
`asyncpg-bank.md` = every failing case verbatim (nodeid + exact db9 error), grouped
by root cause with the owning issue. Query semantics are never masked. Cross-confirms
the connect-path gaps found by the psycopg and pgx lanes from a third impl.

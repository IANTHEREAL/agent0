---
name: pg-tests-case-porting
description: "Select, sanitize, de-dup, and port SQL cases from pg_tests PR#58 into db9 SQL integration tests (tests/*.sql + *.expected), with integration_test.py-compatible expected generation and verification."
---

# pg_tests case porting → db9 integration tests

Port focused SQL cases from `pg_tests` PR#58 into `db9` SQL integration tests (`tests/*.sql` + `tests/*.expected`).

## Purpose & inputs

Inputs:
- A candidate SQL “case” from `pg_tests` PR#58 (query + required setup).
- `PG_EXPECT_DSN`: reference Postgres DSN used ONLY to generate `.expected` (e.g. `postgres://admin:admin@127.0.0.1:38125/postgres`).
- `DB9_DSN`: db9/db9-server DSN used for compatibility verification (e.g. `postgres://admin:admin@127.0.0.1:5433/postgres`).
- Target location: `tests/NN_<topic>.sql` + `tests/NN_<topic>.expected` (optional: `*_setup.sql`).

Goal:
- Create a **self-contained**, **safe**, **deterministic** test that cleanly passes via `scripts/integration_test.py`.

## Hard safety filters (reject / rewrite)

Hard reject anything that can escape the DB sandbox, touch the filesystem, or run arbitrary code:

- **No `psql` meta-commands**: any line starting with `\\` (examples: `\\!`, `\\copy`, `\\o`, `\\i`, `\\ir`, `\\gexec`, `\\set*`, `\\prompt`, `\\watch`, `\\setenv`).
- **No procedural blocks**: `DO $$ ... $$`, anonymous code blocks, or untrusted languages.
- **No file I/O**:
  - `COPY ... TO/FROM 'path'`
  - `COPY ... PROGRAM '...'`
  - `pg_read_file`, `pg_ls_dir`, `lo_import`, etc.
- **No multi-session/concurrency dependencies** (single `psql -f` run per test file).
- **No environment introspection**: relying on `current_user`, `inet_client_addr()`, server version strings, etc. (unless the output is explicitly the thing being tested and is stable in db9).

Quick screens:
```bash
rg -n '^\\s*\\\\' tests_pending/ tests/ || true
rg -n '(?i)\\bDO\\b\\s*\\$\\$|\\bCOPY\\b.*\\bPROGRAM\\b|\\bCOPY\\b.*\\bTO\\b\\s*\\x27|\\bCOPY\\b.*\\bFROM\\b\\s*\\x27' tests_pending/ tests/ || true
```

## Determinism rules (make diffs stable)

- Always add `ORDER BY` for any multi-row query (and prefer explicit column lists over `SELECT *`).
- Avoid nondeterministic functions/values: `random()`, `now()`, `clock_timestamp()`, `txid_current()`, sequences without fixed ordering, etc.
- Avoid plan-dependent output (`EXPLAIN`) unless you are explicitly testing planner output (generally skip these when porting).
- Keep types/formatting stable:
  - Prefer fixed literals (e.g., `DATE '2020-01-01'`, `TIMESTAMP '2020-01-01 00:00:00'`).
  - If timestamps are involved, set a fixed timezone early: `SET TIME ZONE 'UTC';`
- Prefer **small, local schemas** and **unique object names** (e.g., `t_pgtests_<case>`), then `DROP` them.
- If ordering is inherently unstable, use `# unordered` as the **first non-empty line** in the `.expected` file (last resort).

## De-dup checklist (don’t re-add an existing test)

Before writing a new test, search for similar coverage:

```bash
rg -n \"<key_function_or_feature>\" tests/ tests_pending/ || true
rg -n \"<expected_error_substring>\" tests/ tests_pending/ || true
ls tests | rg -n \"<topic>\" || true
```

If the behavior is already tested, prefer extending the existing test file rather than adding a near-duplicate.

## Porting template (setup → action → verify → cleanup)

Use this structure to keep cases readable and safe:

```sql
-- Case: <short description> (ported from pg_tests PR#58)
-- SETUP
DROP TABLE IF EXISTS t_pgtests_<case>;
CREATE TABLE t_pgtests_<case>(...);
INSERT INTO t_pgtests_<case> VALUES (...);

-- ACTION
<statement under test>;

-- VERIFY (prefer SELECTs with ORDER BY)
SELECT ... FROM ... ORDER BY ...;

-- CLEANUP (leave the DB clean for later tests)
DROP TABLE IF EXISTS t_pgtests_<case>;
```

Notes:
- Keep each test file independent; don’t rely on state from other files.
- If you need heavy setup, put it in `tests/NN_<topic>_setup.sql` (the runner will auto-run it first).

## Expected generation (match `scripts/integration_test.py` `psql` flags)

`integration_test.py` runs `psql` with `-X -q -P pager=off` and captures `stderr` into `stdout`.
Generate `.expected` with the same formatting you intend the runner to use:

Unaligned (most common; stable and diff-friendly):
```bash
PGOPTIONS='-c client_min_messages=warning' \
psql "$PG_EXPECT_DSN" -X -q -P pager=off -P format=unaligned -P fieldsep='|' -P null='NULL' \
  -f tests/NN_<topic>.sql > tests/NN_<topic>.expected 2>&1
```

Aligned (default `psql` tables; runner switches to this when `.expected` looks aligned):
```bash
PGOPTIONS='-c client_min_messages=notice' \
psql "$PG_EXPECT_DSN" -X -q -P pager=off \
  -f tests/NN_<topic>.sql > tests/NN_<topic>.expected 2>&1
```

Tips:
- If you want NOTICE output in unaligned mode, keep the output unaligned but generate with `client_min_messages=notice` and ensure `NOTICE:` appears in `.expected` (the runner will then allow notices).
- Prefer fixing row order in SQL over using `# unordered`.

## Verification (runner)

Run a single test against reference Postgres (sanity: `.expected` matches):
```bash
python3 scripts/integration_test.py --dsn "$PG_EXPECT_DSN" tests/NN_<topic>.sql -v
```

Run the same test against db9/db9-server (real compatibility):
```bash
python3 scripts/integration_test.py --dsn "$DB9_DSN" tests/NN_<topic>.sql -v
```

Run the full SQL test suite (optional):
```bash
python3 scripts/integration_test.py --dsn "$DB9_DSN" tests/
```

## PR checklist + template

Checklist:
- [ ] Case passes safety filters (no `\\*`, no `DO`, no file/program `COPY`).
- [ ] Deterministic output (`ORDER BY`, fixed literals; no nondeterministic functions).
- [ ] De-duped against `tests/` and `tests_pending/`.
- [ ] `.expected` generated with `integration_test.py`-compatible `psql` flags.
- [ ] Verified with `python3 scripts/integration_test.py --dsn "$PG_EXPECT_DSN" tests/NN_<topic>.sql` and `--dsn "$DB9_DSN"`.

PR title:
- `test: port pg_tests PR#58 <topic>` (or batch ports: `test: port pg_tests PR#58 cases`)

PR body (starter):
```text
Ports <N> SQL case(s) from pg_tests PR#58 into db9 integration tests under tests/.

Notes:
- Applied safety filters (no psql meta-commands, no DO, no file I/O / COPY PROGRAM).
- Ensured deterministic output (ORDER BY, fixed literals) and generated .expected via integration_test.py-compatible psql flags.
```

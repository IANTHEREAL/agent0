# `auto_testing/corpora` — differential corpus framework (题库)

Runs external **test corpora ("题库")** against db9 and gates regressions, via a
frozen **baseline ratchet**. First corpus is PostgreSQL's own `pg_regress`
(issue #2642); the structure is built so adding more corpora is a new folder, not
new engine code.

## Mental model

- **Engine** (`_engine/`) — *how* to run a class of corpus. Written once, shared.
- **Corpus / 题库** (`<name>/`) — *one* pinned bank of cases + its frozen baseline.
- **Baseline ratchet** — every gated statement has an expected verdict
  (`pass` or `gap`→issue). CI/e2e re-runs db9 and compares:
  - a `pass` that now fails → **regression** → exit non-zero (gate red);
  - a `gap` that now passes → **improvement** → run `--accept` to lock it in.

The gate runs **db9 only** (fast, no PostgreSQL needed). PostgreSQL is used only
when *building* the baseline (`--regen-baseline`).

```
corpora/
  _engine/
    run_corpus.py          # entry: reads manifest, dispatches to a runner, builds/checks baseline
    runners/
      regress_diff.py      # runner for pg_regress-style SQL corpora (split → run → diff)
    lib/
      sqlsplit.py          # SQL statement splitter
      classify.py          # gap → feature-point id (for issue linking)
      baseline.py          # build / compare / accept the ratchet
  pg_regress_17_10/        # corpus #1
    manifest.yaml          # describes this corpus (type, paths, oracle, baseline)
    source/                # vendored, pinned SQL + data + parallel_schedule + CHECKSUMS
    fixtures/              # adapted_setup.sql (portable shared fixtures)
    schedule/              # full_schedule.list + smoke_subset.list
    baseline.json          # the frozen ratchet
    issue_links.json       # feature-point id → tracking issue number
```

## Run it (e2e)

Registered as e2e suites in `scripts/e2e_tests.sh`:

```bash
PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres bash scripts/e2e_tests.sh pg_regress        # regress, smoke
PG_DSN=...                                            bash scripts/e2e_tests.sh pg_regress_full    # regress, full
PG_DSN=...                                            bash scripts/e2e_tests.sh sqllogictest        # output-equivalence, smoke
PG_DSN=...                                            bash scripts/e2e_tests.sh sqllogictest_full   # output-equivalence, full
```

Two corpora are wired today: **pg_regress** (does the statement *run* like PG —
feature coverage) and **sqllogictest** (does the query *return PG's answer* —
value correctness, comparing row-SETS so unspecified `ORDER BY` tie-order isn't a
false diff).

Or directly:

```bash
cd auto_testing/corpora
python3 _engine/run_corpus.py --corpus pg_regress_17_10 --dsn "$PG_DSN" --db-name pgcompat_regress --reset-db --check-baseline --smoke
python3 _engine/run_corpus.py --corpus pg_regress_17_10 --dsn "$PG_DSN" --db-name pgcompat_regress --reset-db --accept --smoke   # after a fix
python3 _engine/run_corpus.py --corpus pg_regress_17_10 --dsn "$PG_DSN" --db-name pgcompat_regress --reset-db --regen-baseline --full \
        --oracle-dsn "host=127.0.0.1 port=55432 dbname=regression user=postgres password=postgres"
```

`--reset-db` drops+recreates a **dedicated** gate database (`--db-name`) for a
clean start every run — this is required, since re-running on a dirty database
hits `CREATE ... already exists` and cascades into false regressions. (The `pg_regress`
e2e suite passes these automatically.)

Requires `psycopg` (`pip install --user "psycopg[binary]"`). `--regen-baseline`
additionally needs a PG 17.10 oracle (Docker `postgres:17.10` on :55432).

## Lifecycle (how it ties to the fix issues)

1. A dev/agent fixes a gap (say #2647, "No PK"). The blocked statements now pass.
2. The gate run prints `IMPROVEMENTS: N statements #2647` (it doesn't fail on these).
3. They run `--accept` → those statements flip `gap`→`pass` in `baseline.json`,
   committed alongside the fix. From now on they are guarded against regression.
4. If anyone later breaks a `pass` statement → the gate goes red.

## Adding a new corpus (题库) — 3 steps, no engine change

1. `mkdir corpora/<new-corpus>/`, drop in the pinned source.
2. Write `manifest.yaml` (set `type:` to an existing runner, or add a runner under
   `_engine/runners/` for a new corpus shape — e.g. `sqllogictest`, `upstream_suite`).
3. Build the first baseline (`--regen-baseline`) and register a suite name in
   `scripts/e2e_tests.sh`.

**Corpus families & which runner they use:**

| Family | Examples | Runner |
|---|---|---|
| SQL differential (run SQL, diff vs PG) | pg_regress, SQLancer | `regress_diff` |
| SQL output-equivalence vs PG | sqllogictest | `sqllogictest` ✅ |
| Run an upstream test suite + blocklist | pgx, Django, SQLAlchemy | `upstream_suite` *(todo, or reuse existing `e2e/<suite>`)* |

All families share the same idea: **pin the source, freeze a baseline that links
failures to issues, gate regressions.**

## Speed / tiering

Not in per-PR CI (too slow). Tiers:

| Tier | What | When |
|---|---|---|
| `pg_regress` (smoke) | curated subset (~hundreds of stmts) | on-demand when touching `src/sql/**` |
| `pg_regress_full` | whole corpus | nightly / pre-release |

## Notes / caveats

- The baseline is **db9-version-specific**. Generate/`--accept` it against the
  db9 build you intend to gate (a local build, not a shared dev endpoint).
- Non-gatable statements are excluded automatically: psql meta-commands,
  `COPY FROM STDIN`, `VACUUM/ANALYZE/...` maintenance, any timeout/connection
  outcome, and `25P02` in-transaction cascades (a statement that only fails
  because an earlier one in its transaction did) — so the ratchet stays
  deterministic even against a shared/busy server.
- The prototype that produced the issue catalog lives in `auto_testing/pg_compat/`
  (kept for the issue repro commands); this `corpora/` tree is the productised home.

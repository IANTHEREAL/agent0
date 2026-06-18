# Django compatibility lane (lane A-ORM)

Runs **Django's own test suite** against db9 via the stock `postgresql` wire
protocol. The blocklist of failures IS the gap registry — see `Django-bank.md`
(auto-generated) and umbrella issue **#2708**.

## Run it
```bash
# needs a running db9 (see the local-db9-standup recipe) + python3/git/psql
bash auto_testing/corpora/django/run.sh                # smoke tier (8 modules)
bash auto_testing/corpora/django/run.sh full           # all 213 modules (slow)
bash auto_testing/corpora/django/run.sh basic lookup   # explicit modules

# or via the standard e2e entrypoint (db9 location from PG_DSN):
PG_DSN=postgres://admin:admin@127.0.0.1:5455/postgres bash scripts/e2e_tests.sh django
```
First run clones Django (`stable/4.2.x`, pinned, into `$DJANGO_DIR=/tmp/django`)
and builds a venv (`$VENV=/tmp/djvenv`); later runs reuse them.

## Pieces
- `run.sh` — acquire → run modules in isolation → classify.
- `db9_settings.py` — test settings; both aliases point at db9 via `db9_backend`.
- `db9_backend/` — stock postgresql **plus** a few documented workarounds for db9
  **test-harness** gaps (NOT query semantics). Each override names its db9 issue
  (`SET CONSTRAINTS` → #2702, `pg_get_serial_sequence` in setval → #2707). When db9
  fixes the gap, delete the override.
- `classify_failures.py` — parse logs → regenerate `Django-bank.md` (every failing
  case verbatim + root-cause grouping, known gaps annotated with their issue).
- `Django-bank.md` — the report (regenerated each run).
- `DESIGN.md` — lane design + standup recipe.

## Findings model
- **Backend overrides** (`db9_backend/`) = harness gaps adapted around — each a logged db9 issue.
- **Failures in `Django-bank.md`** = real query/semantic gaps — the per-feature findings.
  Never silently passed; query-logic differences are recorded, not masked.

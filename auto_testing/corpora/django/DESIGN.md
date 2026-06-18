# Django test lane — design (lane A-ORM, the big one)

Status: **unblocked — setup verified 2026-06-18.** Both parser gaps (#2683 DEFERRABLE FK,
#2684 operator-class index qualifiers) are merged to master, and Django's stock
`django.db.backends.postgresql` backend now runs **all 18 built-in migrations** (contenttypes,
auth, admin, sessions) against a db9 build that includes them — no custom adapter, no masking.
Remaining work: wire the actual `tests/runtests.py` runner + blocklist (see "Next step").

## Why Django, and what changed

Django's ~17k-test suite is the most representative "real app on db9" workload, and it
exercises a surface the differential lanes (regress, sqllogictest) don't: the full
**ORM + `pg_catalog` introspection + migrations + transactions** stack.

A feasibility probe found Django's schema setup was blocked by exactly **two** db9
parser gaps — `DEFERRABLE` foreign keys (#2683) and operator-class index qualifiers
like `varchar_pattern_ops` (#2684). 32/58 of Django's built-in-migration DDL
statements already worked; only those two modifiers broke. **Both are now merged, so
Django's stock `django.db.backends.postgresql` backend works against db9 directly —
no custom adapter, no masking.** (If a residual gap appears, the backend gets a thin,
*documented* override; each override is logged as a db9 gap, never silent.)

### Verified unblock (2026-06-18)

Against a from-master db9 build (commit `abc86810`) the two formerly-blocking statements
now succeed and round-trip correctly in catalog introspection:

```
CREATE TABLE s_b(a_id int REFERENCES s_a(id) DEFERRABLE INITIALLY DEFERRED);
-- pg_constraint: condeferrable=t, condeferred=t
-- pg_get_constraintdef: FOREIGN KEY (a_id) REFERENCES s_a(id) DEFERRABLE INITIALLY DEFERRED

CREATE INDEX s_like ON s_t (session_key varchar_pattern_ops);
-- pg_indexes.indexdef: CREATE INDEX s_like ON public.s_t USING btree (session_key varchar_pattern_ops)
```

`python -c "call_command('migrate', run_syncdb=True)"` → `DJANGO_MIGRATE_OK`, all 18
migrations applied, 13 tables introspected.

## Runner family

This is the **upstream-suite** family (run someone else's test runner + a
blocklist-with-reasons), like the existing `e2e/sqlalchemy_smoke` — NOT the
differential engine. The blocklist IS the gap registry.

## Workflow

1. **Acquire** (pinned): `git clone --depth 1 -b <tag> https://github.com/django/django` (tag = the installed Django, currently 4.2.x). Vendor nothing; fetch + pin by tag/commit.
2. **Configure**: a test settings module pointing `DATABASES['default']` at a db9 build that includes #2683/#2684, via the stock `postgresql` backend (dedicated gate db, reset each run).
3. **Run**: `python tests/runtests.py --settings=<db9_settings> <apps>`; capture per-test pass/fail (JUnit/`--verbosity`).
4. **Blocklist**: failures → `blocklist.yaml` with a reason each (db9 gap / intended-divergence / Django-PG-internal). Green-or-blocklisted is the gate; new failures = regression.
5. **Tier**: smoke = a few core apps (`basic lookup queries aggregation annotations expressions`); full = the whole suite (nightly/on-demand).
6. **e2e + PR**: register `bash scripts/e2e_tests.sh django[_full]`; ship as its **own PR** (separate from the differential framework PR #2682).

## Standing up a db9 to test against (verified recipe)

The hard part is the environment, not the fixes. A db9 build needs all of the following or
it exits at startup (each was hit in turn during the 2026-06-18 bring-up):

1. **TiKV in API v2** (keyspace-capable) — raw `tiup playground` defaults to API v1 and db9
   dies with `ApiVersionNotMatched { storage_api_version: V1, req_api_version: V2 }`. Use
   `python3 scripts/tikv_admin.py start --persistent --pd-port 2379`.
2. **The `default` keyspace** — `POST http://127.0.0.1:2379/pd/api/v2/keyspaces` body
   `{"name":"default"}` (idempotent; 500 = already exists).
3. **A live redis** — db9 PINGs `REDIS_URL` at startup and fails fast if unreachable
   (`src/extensions/fs/redis_events.rs`). `apt-get install -y redis-server && redis-server --daemonize yes`.
4. **db9 launch env** (matches `scripts/regression_gate.sh`):
   `PD_ENDPOINTS=127.0.0.1:2379 PG_PORT=5433 DB9_BOOTSTRAP_ADMIN_USER=admin
   DB9_BOOTSTRAP_ADMIN_PASSWORD=admin DB9_INSECURE=1 REDIS_URL=redis://127.0.0.1:6379
   ./target/release/db9-server`. (Do **not** also set `DB9_DEV=1` — it demands a separate
   `DB9_DEV_ADMIN_PASSWORD` and conflicts with the bootstrap-admin path.)

This is the same path CI's regression-gate uses; the gate already runs `tests/2683_deferrable_fk.sql`
and `tests/2684_index_opclass.sql` against it.

## Hard dependency: a fixed db9 to test against

Tests can only run against a db9 that has #2683 + #2684 (now merged). Path: either dev1
redeploys with the new build, or build db9 from merged master and run it via the recipe
above. This deploy step is tracked separately (it's the rolling-deploy dependency, not a
harness gap).

## Next step

The setup is proven; what remains is the runner itself:

1. Add `auto_testing/corpora/django/run.sh` (or extend `_engine`) to clone Django at the
   pinned tag, write the db9 settings module, and invoke `tests/runtests.py` for the smoke
   app set.
2. Seed `blocklist.yaml` from the first full run — every failure gets a reason
   (db9 gap → file an issue / intended-divergence / Django-PG-internal).
3. Register `bash scripts/e2e_tests.sh django` and ship as its own PR.

## Findings model (two outputs)

- **Backend overrides** (if any) = DDL/feature gaps Django hit that we adapt around — each a logged db9 gap.
- **Test blocklist** = behavioral gaps after setup succeeds — the real per-feature findings (catalog introspection, ORM semantics, transactions).

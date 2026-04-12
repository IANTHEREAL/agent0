# M1 Benchmark Baseline

**Status**: Draft  
**Author**: AI Assistant  
**Date**: 2026-04-12  
**Scope**: Frozen benchmark corpus, dataset shapes, and reporting contract for `M1` and all later optimization milestones

> **Draft / non-SoT note**
>
> This document defines the benchmark baseline contract for the optimization
> program. It does not override the current implementation truth in `src/**`,
> `docs/sot/**`, or `docs/ARCHITECTURE.md`.

---

## Goal

`M1` must freeze one reusable benchmark corpus that can expose the payoff of
`M2` to `M8` without inventing a brand new workload for every milestone.

The corpus must satisfy four constraints:

- it must contain at least one realistic application query shape per milestone
- it must run unchanged against `db9 before`, `db9 after`, and local
  `PostgreSQL 18.3`
- it must be deterministic enough to compare results across commits
- it must include function-expression filter companions so later optimizer work
  stays compatible with `M0` pushdown instead of only looking good on plain
  column predicates

Companion artifacts:

- generator and harness contract:
  [`benchmark_m1_generator_and_harness.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_generator_and_harness.md)
- raw result JSON schema:
  [`benchmark_m1_result_schema.json`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_result_schema.json)
- TPC side benchmark note:
  [`benchmark_m1_tpc_side_benchmarks.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_tpc_side_benchmarks.md)

## Design Principles

- Freeze a small number of tables and a fixed parameter pack instead of a large
  random SQL zoo.
- Put the primary `M2` to `M4` payoff on one wide, secondary-index-driven
  pagination table so streaming, late materialization, and ordered traversal are
  forced to compete on the same query family.
- Keep one large append-only fact table for stream aggregate, parallel scan, and
  low-memory spill scenarios.
- Keep one small-to-medium ORM table for repeated prepared traffic and
  short-connection plan-cache scenarios.
- For every canonical scenario `Qxx`, define a companion `QxxF` that adds one
  pushdown-eligible function predicate. The `Qxx` line is the primary payoff
  line. The `QxxF` line is the compatibility line.
- Do not let the function companion accidentally destroy the target access path.
  For `M2` to `M4`, function companions must use either an expression index or a
  residual predicate whose selectivity is documented.

## Why This Corpus Can Cover `M2` To `M8`

The optimization plan requires the later milestones to prove gains on:

- streaming and early stop
- fewer fetched base rows and fewer decoded rows
- ordered traversal and real `TopN`
- stream aggregate on ordered input
- plan cache reuse on pooled application traffic
- multi-core speedup on large analytics
- bounded-memory success with visible spill metrics

The corpus in this document uses three benchmark tables:

- `bench_orders`: wide row, hot filtered set, composite secondary index, full-row
  pagination. This is the primary `M2` to `M4` application case.
- `bench_events`: append-only analytics fact table. This is the primary `M5`,
  `M7`, and `M8` table, and also provides a pure streaming `M2` case that does
  not depend on late materialization.
- `bench_users`: medium-size ORM lookup table for `M6`.

Together they are enough to expose every required milestone payoff without
forcing the team to maintain many unrelated datasets.

## Side Benchmarks

`TPC-C` and `TPC-H` should be part of the `M1` benchmark inventory, but they
should not replace the canonical scenario families in this document.

Positioning:

- the canonical `Q01` to `Q10` corpus remains the blocking milestone gate
- `TPC-C` is the side benchmark for end-to-end OLTP behavior, prepared
  execution, and pooled application traffic
- `TPC-H` is the side benchmark for large joins, aggregates, sort-heavy
  analytics, and later `M5` to `M8` scale-up work

Rule:

- if the canonical corpus and a TPC side benchmark disagree, milestone close is
  still blocked by the canonical corpus
- TPC side benchmarks are additive evidence, not a replacement contract

See the detailed runbook in
[`benchmark_m1_tpc_side_benchmarks.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_tpc_side_benchmarks.md).

## Primary `M2` To `M4` Application Case

The key application shape for `M2` to `M4` is:

```sql
SELECT *
FROM bench_orders
WHERE tenant_id = 1
  AND status = 2
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

The large dataset must guarantee that:

- `tenant_id = 1 AND status = 2` qualifies about `100k` rows on the large scale
- the row is wide enough that back-to-table fetch is expensive
- the composite index `(tenant_id, status, created_at DESC, order_id DESC)`
  exists

This one shape is intentionally staged so every later milestone can improve it:

- Before `M2`: the engine may materialize or buffer far too much work before the
  first row becomes visible.
- After `M2`: first-row latency and peak RSS should drop, but the engine may
  still fetch or decode far more than `10` full rows.
- After `M3`: narrow-projection variants should avoid many base-row fetches.
- After `M4`: ordered traversal plus early stop should reduce qualifying index
  work and full-row fetch work toward `LIMIT`, not toward the full `100k`
  qualifying set.

The function companion for this family is:

```sql
SELECT *
FROM bench_orders
WHERE tenant_id = 1
  AND status = 2
  AND lower(channel) = 'push'
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

The large dataset must also guarantee that:

- `tenant_id = 1 AND status = 2 AND lower(channel) = 'push'` still qualifies a
  large set, with the target around `60k`
- an expression index exists so this companion does not turn into a different
  benchmark family

The intended staged effect is:

- pre-optimization baseline: up to the full qualifying set may be fetched or
  decoded
- `M2` only: first-row latency improves, but fetched base rows may remain high
- `M3` plus `M4`: secondary index work and back-table fetches should trend
  toward `10`

## Dataset Scales

The dataset must be frozen at three scales.

| Scale | `bench_users` | `bench_orders` | `bench_events` | Main use |
| --- | ---: | ---: | ---: | --- |
| `S` | `100k` | `200k` | `1M` | PR smoke and local debugging |
| `M` | `250k` | `800k` | `5M` | daily perf and pre-merge validation |
| `L` | `500k` | `2M` | `10M` | milestone-close evidence on `8c36g` EC2 |

Large-scale requirements:

- `bench_orders` row width target: `1.2KB` to `2.0KB`
- `bench_events` row width target: `300B` to `700B`
- `bench_users` row width target: `200B` to `500B`
- total on-disk footprint should remain reasonable for one `8c36g` machine with
  attached EBS, but large enough that in-memory full-sort or full-fetch
  behavior is visible

## Server Acceptance Dataset Policy

The benchmark server used for CI or milestone-close acceptance must not rebuild
datasets in the hot path.

Rules:

- each benchmark dataset artifact used for server-side acceptance must target
  `10GB` of prepared data
- this rule applies to the canonical `M1` corpus and to any approved side
  benchmark suites such as `TPC-C` or `TPC-H`
- the exact measured size should be recorded in the evidence bundle
- a small drift around the target is acceptable, but the intended contract is a
  fixed `10GB` class dataset, not ad hoc row counts per run

Operational policy:

- prepare the dataset once on the benchmark server
- run `ANALYZE` or the engine-equivalent statistics preparation once
- take a backup or snapshot immediately after preparation
- before each measured benchmark run, restore from that prepared backup instead
  of regenerating data
- the measured benchmark window must start after restore completes, not during
  data generation

This is required so benchmark time measures query execution behavior rather than
dataset creation cost.

## Data Distribution Contract

The generator must be deterministic and seeded. The seed is part of the
benchmark evidence bundle.

### Tenant Skew

- use `1000` tenants
- `tenant_id = 1` owns `25%` of `bench_orders` and `20%` of `bench_events`
- tenants `2` to `10` together own another `35%`
- the long tail owns the remaining `40%`

### Orders Distribution

- `status = 2` occurs on `20%` of rows for `tenant_id = 1`
- that makes `tenant_id = 1 AND status = 2` about `100k` rows on large scale
- within that hot pair, `channel` values are skewed so `lower(channel) = 'push'`
  still returns roughly `60k` rows
- `created_at` is append-only and highly correlated with `order_id`
- `attrs` and `note` must be wide enough that full-row fetch cost is visible

### Events Distribution

- `created_at` is append-only and highly correlated with `event_id`
- `created_day` spans enough days that ordered group-by by day is meaningful
- `country` and `device_type` are low-cardinality grouping dimensions
- `channel` and `priority_bucket` are used for function companion filters
- `score` is intentionally weakly correlated with `created_at` so low-memory sort
  still has to do real work

### Users Distribution

- `bench_users` supports repeated point lookup, repeated email lookup, and mixed
  hot/cold tenant traffic
- `tenant_id = 1` should be hot enough to expose plan-cache wins under pooled
  traffic

## Schema

### `bench_orders`

```sql
CREATE TABLE bench_orders (
    order_id BIGINT PRIMARY KEY,
    tenant_id INT NOT NULL,
    status SMALLINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    created_day DATE NOT NULL,
    user_id BIGINT NOT NULL,
    channel TEXT NOT NULL,
    country TEXT,
    priority_bucket INT NOT NULL,
    amount_cents BIGINT NOT NULL,
    score DOUBLE PRECISION NOT NULL,
    title TEXT NOT NULL,
    note TEXT NOT NULL,
    attrs JSONB NOT NULL
);

CREATE INDEX bo_tenant_status_created_idx
    ON bench_orders (tenant_id, status, created_at DESC, order_id DESC);

CREATE INDEX bo_tenant_status_lower_channel_created_idx
    ON bench_orders (tenant_id, status, lower(channel), created_at DESC, order_id DESC);

CREATE INDEX bo_tenant_status_abs_priority_created_idx
    ON bench_orders (tenant_id, status, abs(priority_bucket), created_at DESC, order_id DESC);
```

`bench_orders` is the main `M2` to `M4` table. It must stay wide.

Recommended payload targets:

- `title`: `64B` to `128B`
- `note`: `256B` to `512B`
- `attrs`: `512B` to `1024B`

### `bench_events`

```sql
CREATE TABLE bench_events (
    event_id BIGINT PRIMARY KEY,
    tenant_id INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    created_day DATE NOT NULL,
    user_id BIGINT NOT NULL,
    event_type SMALLINT NOT NULL,
    channel TEXT NOT NULL,
    country TEXT,
    device_type TEXT NOT NULL,
    priority_bucket INT NOT NULL,
    amount_cents BIGINT NOT NULL,
    score DOUBLE PRECISION NOT NULL,
    body TEXT NOT NULL,
    attrs JSONB NOT NULL
);

CREATE INDEX be_tenant_created_idx
    ON bench_events (tenant_id, created_at DESC, event_id DESC);

CREATE INDEX be_tenant_lower_channel_created_idx
    ON bench_events (tenant_id, lower(channel), created_at DESC, event_id DESC);

CREATE INDEX be_tenant_created_day_country_device_idx
    ON bench_events (tenant_id, created_day, country, device_type, event_id);

CREATE INDEX be_tenant_lower_channel_created_day_country_device_idx
    ON bench_events (tenant_id, lower(channel), created_day, country, device_type, event_id);

CREATE INDEX be_tenant_abs_priority_created_idx
    ON bench_events (tenant_id, abs(priority_bucket), created_at DESC, event_id DESC);
```

`bench_events` is the shared table for `M2`, `M5`, `M7`, and `M8`.

Recommended payload targets:

- `body`: `96B` to `192B`
- `attrs`: `128B` to `512B`

### `bench_users`

```sql
CREATE TABLE bench_users (
    user_id BIGINT PRIMARY KEY,
    tenant_id INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    status SMALLINT NOT NULL,
    region TEXT NOT NULL,
    email TEXT NOT NULL,
    plan_code TEXT NOT NULL,
    profile JSONB NOT NULL
);

CREATE INDEX bu_tenant_user_idx
    ON bench_users (tenant_id, user_id);

CREATE INDEX bu_tenant_lower_email_idx
    ON bench_users (tenant_id, lower(email));
```

## Canonical Scenarios

The corpus freezes `10` canonical scenario families. Each family has a plain
predicate query `Qxx` and a function companion `QxxF`.

| ID | Milestone focus | Table | Shape | Main improvement signal |
| --- | --- | --- | --- | --- |
| `Q01` | `M2` | `bench_orders` | hot filtered first page, full row | `first_row_latency`, `peak_rss` |
| `Q02` | `M2` | `bench_events` | append-only first page | `first_row_latency`, `rows_scanned_before_stop` |
| `Q03` | `M3` | `bench_orders` | narrow projection on hot filtered page | `fetched_base_rows`, `decoded_rows` |
| `Q04` | `M4` | `bench_orders` | ordered `LIMIT` | `sort_bytes`, `kv_requests`, `payload_bytes` |
| `Q05` | `M4` | `bench_orders` | ordered `OFFSET + LIMIT` | `rows_scanned_before_stop`, `fetched_base_rows` |
| `Q06` | `M5` | `bench_events` | ordered `GROUP BY` | `aggregate_memory`, `elapsed` |
| `Q07` | `M6` | `bench_users` | repeated prepared lookup | `plan_cache_hit_ratio`, `planning_cpu` |
| `Q08` | `M7` | `bench_events` | large analytics aggregate | `speedup_curve`, `cpu_time` |
| `Q09` | `M8` | `bench_events` | low-memory sort | `spill_bytes`, `completion_rate` |
| `Q10` | `M8` | `bench_events` | low-memory aggregate | `spill_bytes`, `aggregate_memory` |

### Function Companion Rule

For every `Qxx`, `QxxF` must:

- keep the same projection, ordering, and limit shape
- add exactly one pushdown-eligible function predicate
- use the same parameter pack across `db9 before`, `db9 after`, and local
  `PostgreSQL 18.3`
- be reported in the same result table as the base query

The base line proves the optimization payoff. The function line proves the
optimization did not regress compatibility with `M0` pushdown.

## Canonical SQL

Parameter pack conventions:

- `:tenant_hot = 1`
- `:status_hot = 2`
- `:channel_push = 'push'`
- `:priority_abs = 7`
- `:offset_deep = 10000`

### `Q01` `orders_page_full_row`

```sql
SELECT *
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

`Q01F`

```sql
SELECT *
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
  AND lower(channel) = $3
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

Large-scale target parameters:

- `Q01`: `(1, 2)`
- `Q01F`: `(1, 2, 'push')`

Expected staged effect:

- `M2`: lower first-row latency and bounded buffering
- `M3`: little change for `SELECT *`
- `M4`: qualifying index work and full-row fetch work trend toward `10`

### `Q02` `events_stream_first_page`

```sql
SELECT *
FROM bench_events
WHERE tenant_id = $1
ORDER BY created_at DESC, event_id DESC
LIMIT 10;
```

`Q02F`

```sql
SELECT *
FROM bench_events
WHERE tenant_id = $1
  AND abs(priority_bucket) = $2
ORDER BY created_at DESC, event_id DESC
LIMIT 10;
```

Execution notes:

- the harness must also run an early-close variant where the client stops after
  reading `3` rows
- this is the pure `M2` case that is not blocked on `M3` or `M4`

### `Q03` `orders_page_narrow_projection`

```sql
SELECT order_id, created_at, amount_cents
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

`Q03F`

```sql
SELECT order_id, created_at, amount_cents
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
  AND lower(channel) = $3
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

This is the primary `M3` case.

Expected staged effect:

- before `M3`: the engine may still fetch or decode many full rows
- after `M3`: fetched base rows and decoded rows should drop sharply
- after `M4`: the remaining index work should also trend toward `10`

### `Q04` `orders_ordered_topn`

```sql
SELECT order_id, created_at, amount_cents, score
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

`Q04F`

```sql
SELECT order_id, created_at, amount_cents, score
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
  AND lower(channel) = $3
ORDER BY created_at DESC, order_id DESC
LIMIT 10;
```

This is the primary `M4` `ORDER BY ... LIMIT` case.

### `Q05` `orders_ordered_offset`

```sql
SELECT order_id, created_at, amount_cents, score
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
ORDER BY created_at DESC, order_id DESC
LIMIT 10 OFFSET $3;
```

`Q05F`

```sql
SELECT order_id, created_at, amount_cents, score
FROM bench_orders
WHERE tenant_id = $1
  AND status = $2
  AND lower(channel) = $4
ORDER BY created_at DESC, order_id DESC
LIMIT 10 OFFSET $3;
```

Large-scale target parameters:

- `Q05`: `(1, 2, 10000)`
- `Q05F`: `(1, 2, 10000, 'push')`

This case must prove the difference between:

- pre-`M4`: read or sort a very large qualifying set
- post-`M4`: stop after about `offset + limit` qualifying index entries and push
  base-row fetch as late as possible

### `Q06` `events_ordered_group_by`

```sql
SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents)
FROM bench_events
WHERE tenant_id = $1
GROUP BY created_day, country, device_type
ORDER BY created_day, country, device_type;
```

`Q06F`

```sql
SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents)
FROM bench_events
WHERE tenant_id = $1
  AND lower(channel) = $2
GROUP BY created_day, country, device_type
ORDER BY created_day, country, device_type;
```

This is the primary `M5` stream-aggregate case.

### `Q07` `users_prepared_lookup`

```sql
SELECT user_id, status, region, plan_code
FROM bench_users
WHERE tenant_id = $1
  AND user_id = $2;
```

`Q07F`

```sql
SELECT user_id, status, region, plan_code
FROM bench_users
WHERE tenant_id = $1
  AND lower(email) = $2;
```

Execution notes:

- run at least `10k` repeated parameterized executions
- run both a stable hot-tenant parameter set and a mixed hot/cold tenant set
- report parse or analyze CPU separately when available

### `Q08` `events_parallel_analytics`

```sql
SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score)
FROM bench_events
WHERE tenant_id = $1
  AND created_day BETWEEN $2 AND $3
GROUP BY created_day, country, device_type
ORDER BY created_day, country, device_type;
```

`Q08F`

```sql
SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score)
FROM bench_events
WHERE tenant_id = $1
  AND created_day BETWEEN $2 AND $3
  AND lower(channel) = $4
GROUP BY created_day, country, device_type
ORDER BY created_day, country, device_type;
```

This is the primary `M7` scale-up case. It should run at `1`, `2`, `4`, and
`8` workers or equivalent parallelism levels.

### `Q09` `events_spill_sort`

```sql
SELECT event_id, created_at, score
FROM bench_events
WHERE tenant_id = $1
ORDER BY score DESC, event_id DESC
LIMIT 50000;
```

`Q09F`

```sql
SELECT event_id, created_at, score
FROM bench_events
WHERE tenant_id = $1
  AND abs(priority_bucket) = $2
ORDER BY score DESC, event_id DESC
LIMIT 50000;
```

This is the primary `M8` spill-sort case. It must run under deliberately tight
memory settings.

### `Q10` `events_spill_aggregate`

```sql
SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score)
FROM bench_events
WHERE tenant_id = $1
GROUP BY user_id
ORDER BY user_id;
```

`Q10F`

```sql
SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score)
FROM bench_events
WHERE tenant_id = $1
  AND lower(channel) = $2
GROUP BY user_id
ORDER BY user_id;
```

This is the primary `M8` spill-aggregate case.

## Scenario-Specific Counter Expectations

The report must not stop at elapsed time.

| ID | Must improve by milestone close |
| --- | --- |
| `Q01`, `Q02` | `first_row_latency`, `peak_rss`, `rows_scanned_before_stop` |
| `Q03` | `fetched_base_rows`, `decoded_rows`, `payload_bytes` |
| `Q04`, `Q05` | `sort_bytes`, `payload_bytes`, `kv_requests`, `rows_scanned_before_stop` |
| `Q06` | `aggregate_memory`, `elapsed`, `p95` |
| `Q07` | `plan_cache_hit_ratio`, `planning_cpu`, `p50`, `p95` |
| `Q08` | `elapsed`, `cpu_time`, speedup versus worker count |
| `Q09`, `Q10` | successful completion under low memory, `spill_bytes`, `passes`, `cleanup` |

## Harness Contract

The harness must be one reusable driver, not milestone-specific shell fragments.

Minimum contract:

- same driver for `db9 before`, `db9 after`, and local `PostgreSQL 18.3`
- same schema and logically equivalent indexes across all three
- same seed and same generated data
- same query text or same prepared-statement shape
- same warmup policy and run count
- same parameter pack
- raw per-run results attached

## PostgreSQL Reference Line

The local `PostgreSQL 18.3` line is mandatory and blocking for every canonical
scenario family. It is not optional supporting data.

Comparison rules:

- `db9_before`, `db9_after`, and `postgres_18_3` must use the same scale, seed,
  dataset shape, parameter-pack ids, warmup policy, and measured-run count
- schema must be the same, except for engine-specific DDL that is strictly
  required to express the same logical index shape
- function companion cases `QxxF` must also run on PostgreSQL, not only the base
  `Qxx` cases
- when PostgreSQL cannot expose a db9-specific internal metric such as
  `kv_requests`, the result column must still exist and be recorded as `null`
- every milestone-close report must include one short note for any large db9 to
  PostgreSQL gap, explaining the most likely remaining bottleneck

The purpose of the PostgreSQL line is:

- semantic reference for visible results
- performance reference for realistic expectations
- regression control so db9 gains are not measured only relative to its own
  older baseline

Recommended run policy:

- warmup: `3`
- measured runs: `7`
- latency scenarios: single query stream unless the scenario explicitly models
  pooled application traffic
- `Q07`: at least `32` client concurrency in the pooled run
- `Q08`: run at `1`, `2`, `4`, and `8` worker levels
- `Q09` and `Q10`: run at two memory levels such as `64MB` and `128MB`

## Reporting Contract

Every result row must include:

- benchmark id
- engine variant: `db9_before`, `db9_after`, or `postgres_18_3`
- scale: `S`, `M`, or `L`
- seed
- parameter pack id
- elapsed time
- `p50`
- `p95`
- first-row latency when relevant
- peak RSS
- CPU time when available
- scanned rows
- decoded rows
- fetched base rows
- payload bytes
- KV requests
- sort bytes
- aggregate memory
- spill bytes
- spill passes
- plan-cache hit ratio

If a field is not available on PostgreSQL, keep the column and record `null`.

## Milestone-To-Scenario Matrix

| Milestone | Required primary scenarios | Required compatibility scenarios |
| --- | --- | --- |
| `M2` | `Q01`, `Q02` | `Q01F`, `Q02F` |
| `M3` | `Q03` | `Q03F` |
| `M4` | `Q04`, `Q05` | `Q04F`, `Q05F` |
| `M5` | `Q06` | `Q06F` |
| `M6` | `Q07` | `Q07F` |
| `M7` | `Q08` | `Q08F` |
| `M8` | `Q09`, `Q10` | `Q09F`, `Q10F` |

Rule:

- A milestone may add exploratory micro-benchmarks, but it cannot replace the
  canonical scenario family above.
- A milestone that improves `Qxx` but regresses `QxxF` does not pass.

## Server Baseline Workflow

The benchmark server must keep an accepted baseline for each suite.

Minimum workflow:

1. Prepare the canonical benchmark dataset on the benchmark server.
2. Back up the prepared dataset in its fixed `10GB` form.
3. Run the accepted baseline build and store the raw result plus compare report.
4. For each new milestone, restore the prepared dataset before the run.
5. Compare the new run against the last accepted server baseline from the same
   suite.
6. Only after the milestone is accepted should the new result become the next
   accepted baseline.

Baseline comparison rules:

- compare on the same benchmark server or equivalent hardware class
- compare on the same `10GB` dataset artifact
- compare with the same query set, parameter packs, and warmup policy
- compare against the previous accepted milestone result, not just against an
  arbitrary recent branch run

## Explicit `M0` Pushdown Compatibility Requirement

The function companions are not optional decoration.

They exist to prevent the later milestones from accidentally:

- bypassing pushdown on function predicates that are already supported
- forcing local fallback for function filters that used to be safe
- making ordered traversal or late materialization work only for plain column
  predicates
- hiding regressions behind a new path that only looks good on the simplest
  cases

The benchmark gate therefore has two acceptance lines:

- payoff line: the base scenario improves on its target counters
- compatibility line: the function companion remains correct and does not lose
  the expected access-path benefit without an explicit, documented reason

## Acceptance For `M1`

`M1` is complete for benchmarking purposes only when:

- the three benchmark tables and the frozen data scales are documented
- the deterministic generator rules are documented
- the `10` canonical scenario families and their function companions are frozen
- the large-scale `bench_orders` hot pair still produces about `100k`
  qualifying rows
- the reporting schema is frozen
- the milestone-to-scenario mapping is frozen
- the harness inventory exists and can target `db9 before`, `db9 after`, and
  local `PostgreSQL 18.3`

## Non-Goals

- This document does not choose the implementation language of the harness.
- This document does not define the final metric names exported by code. It
  defines the required benchmark-facing fields.
- This document does not require every future milestone to beat PostgreSQL. It
  requires a stable comparison line and root-cause explanation for large gaps.
- This document does not make `TPC-C` or `TPC-H` the primary acceptance gate.
  They remain side benchmarks.

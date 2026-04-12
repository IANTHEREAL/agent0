# M1 Benchmark Generator And Harness Contract

**Status**: Draft  
**Author**: AI Assistant  
**Date**: 2026-04-12  
**Scope**: Deterministic dataset generator rules, parameter packs, command
convention, and raw result layout for the `M1` benchmark corpus

> **Draft / non-SoT note**
>
> This document defines how the benchmark corpus should be generated and
> executed. It does not imply that the script already exists. It freezes the
> contract that the future harness must follow.

---

## Purpose

[`benchmark_m1_baseline.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_baseline.md)
freezes the benchmark corpus. This companion document freezes how that corpus is
generated and how the harness is expected to run it.

The design goal is simple:

- one deterministic dataset generator
- one reusable benchmark runner
- one compare step
- one raw JSON output schema

## Reserved Harness Entry Point

The reserved harness entry point is:

```bash
python3 scripts/benchmark_m1_baseline.py <subcommand> ...
```

This document does not require Python specifically, but if a different
implementation language is chosen later, it must preserve the same subcommand
shape and JSON output contract.

Reserved subcommands:

- `gen`: create schema and load deterministic data
- `run`: execute one engine variant against one scale and one scenario set
- `compare`: merge or compare `db9_before`, `db9_after`, and `postgres_18_3`
  raw outputs into one report

Server-side restore-first workflow:

- the benchmark server should prepare datasets once, back them up, and restore
  them before each measured run
- the current harness may rely on an external restore orchestration step until a
  dedicated restore subcommand is added
- data generation is allowed for local development and one-time dataset
  preparation, but not for the hot path of server-side acceptance runs

Optional metrics capture:

- the harness may scrape a Prometheus endpoint such as
  `http://127.0.0.1:9090/internal/metrics`
- selected Prometheus series may be captured by name and stored as per-repeat
  delta metrics in the raw output
- if the captured alias matches a canonical benchmark counter name such as
  `kv_requests`, `spill_bytes`, or `plan_cache_hit_ratio`, the harness should
  populate that canonical field directly
- otherwise the harness should store the value under `extra_metrics`

External side benchmark inventory:

- `TPC-C` and `TPC-H` are tracked as side benchmarks for `M1`, not as the main
  gate
- recommended tool inventory and db9-specific notes live in
  [`benchmark_m1_tpc_side_benchmarks.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_tpc_side_benchmarks.md)
- side benchmark outputs should be stored separately from the canonical
  `Q01`..`Q10` raw result files so the primary acceptance path stays clean

## PostgreSQL Comparison Contract

The harness must treat local `PostgreSQL 18.3` as a first-class benchmark
target, not as an afterthought.

Mandatory rules:

- the harness must support `--engine-label postgres_18_3`
- the same `gen` rules must be applied to PostgreSQL using the same seed and
  scale
- the same canonical scenario ids and parameter-pack ids must run on PostgreSQL
- the same protocol mode should be used when practical; when not practical, the
  chosen deviation must be recorded in raw metadata
- the raw result file must record the exact engine version string reported by
  the server
- the compare stage must refuse to compare files when the scale, seed,
  parameter-pack ids, or scenario ids do not match

Required PostgreSQL setup notes in the evidence bundle:

- PostgreSQL exact version string
- DSN target or host label in redacted form
- settings relevant to benchmark fairness, such as memory knobs or parallelism
  knobs when those are intentionally adjusted
- confirmation that benchmark tables and indexes were created on PostgreSQL with
  the same logical shape

## Generator Contract

### Seed Contract

- top-level benchmark seed is required
- default seed: `20260412`
- the same seed must be used across `db9_before`, `db9_after`, and
  `postgres_18_3`
- the generator must derive stable per-table sub-seeds from:
  `hash(seed, table_name, scale)`

Example derivation rule:

```text
orders_seed = hash64(seed || "bench_orders" || scale)
events_seed = hash64(seed || "bench_events" || scale)
users_seed  = hash64(seed || "bench_users"  || scale)
```

The implementation may choose the exact hash function, but once the harness is
shipped it must not change silently.

### Scale Contract

| Scale | `bench_users` | `bench_orders` | `bench_events` |
| --- | ---: | ---: | ---: |
| `S` | `100000` | `200000` | `1000000` |
| `M` | `250000` | `800000` | `5000000` |
| `L` | `500000` | `2000000` | `10000000` |

For server-side acceptance:

- each prepared benchmark dataset artifact should target `10GB`
- the artifact size should be recorded together with the benchmark result
- the same `10GB` artifact must be restored before each measured benchmark run

### Determinism Rule

The generator must not use sampling rules that only approximately satisfy the
hot-path cardinality goals. The hot-path counts must be constructed
deterministically so the large-scale benchmark remains stable over time.

For large scale:

- `bench_orders` must produce about `100k` rows for
  `tenant_id = 1 AND status = 2`
- `bench_orders` must produce about `60k` rows for
  `tenant_id = 1 AND status = 2 AND lower(channel) = 'push'`

The implementation should therefore use bucketed row assignment rather than pure
probabilistic draws.

## Table Generation Rules

### `bench_orders`

The generator should assign rows in deterministic blocks.

Large-scale reference allocation:

- total rows: `2000000`
- tenant `1`: exactly `500000` rows
- within tenant `1`:
  - exactly `100000` rows with `status = 2`
  - exactly `60000` rows with `status = 2` and `lower(channel) = 'push'`
- tenants `2` to `10`: together exactly `700000` rows
- tenants `11` to `1000`: together exactly `800000` rows

Recommended row construction rules:

- `order_id`: dense monotonic sequence starting at `1`
- `tenant_id`: assigned by deterministic bucket map
- `status`: assigned by per-tenant deterministic bucket map
- `created_at`: `base_ts + order_id seconds + stable small jitter`
- `created_day`: `date(created_at)`
- `user_id`: deterministic mapping into `bench_users`
- `channel`: deterministic categorical distribution from
  `['push', 'email', 'ads', 'organic', 'partner']`
- `country`: deterministic draw from a fixed country list
- `priority_bucket`: integer in `[-10, 10]`
- `amount_cents`: positive skewed numeric value
- `score`: floating-point value with limited correlation to `created_at`
- `title`: deterministic synthetic text
- `note`: deterministic medium text payload
- `attrs`: deterministic JSON payload with stable key order

Payload rule:

- row width must remain intentionally wide
- generated text and JSON must be content-stable under the same seed
- compression-friendly repetition is allowed, but not to the point that full-row
  fetch becomes unrealistically cheap

### `bench_events`

Large-scale reference allocation:

- total rows: `10000000`
- tenant `1`: exactly `2000000` rows
- tenants `2` to `10`: together exactly `3500000` rows
- tenants `11` to `1000`: together exactly `4500000` rows

Recommended row construction rules:

- `event_id`: dense monotonic sequence starting at `1`
- `tenant_id`: deterministic bucket map
- `created_at`: append-only and highly correlated with `event_id`
- `created_day`: derived from `created_at`
- `user_id`: deterministic mapping into the tenant-local user space
- `event_type`: low-cardinality dimension
- `channel`: deterministic categorical distribution
- `country`: low-cardinality dimension with some `NULL`
- `device_type`: deterministic categorical distribution
- `priority_bucket`: integer in `[-10, 10]`
- `amount_cents`: positive skewed numeric value
- `score`: intentionally not strongly ordered by `created_at`
- `body`: deterministic short text payload
- `attrs`: deterministic JSON payload

### `bench_users`

Large-scale reference allocation:

- total rows: `500000`
- tenant `1`: exactly `100000` rows
- tenants `2` to `10`: together exactly `175000` rows
- tenants `11` to `1000`: together exactly `225000` rows

Recommended row construction rules:

- `user_id`: dense monotonic sequence
- `tenant_id`: deterministic bucket map
- `created_at`: deterministic timestamp
- `status`: low-cardinality dimension
- `region`: low-cardinality dimension
- `email`: deterministic unique value, with case patterns stable enough for
  `lower(email)` benchmarks
- `plan_code`: low-cardinality dimension
- `profile`: deterministic JSON payload

## Generator Pseudocode

The future script does not have to follow this exact implementation, but the row
allocation behavior must be equivalent.

```text
for each table T:
  rows = scale_rows[T]
  for row_id in 1..rows:
    tenant_id = tenant_bucket(T, row_id, scale, seed)
    attrs = deterministic_payload(T, row_id, tenant_id, seed)
    emit row
```

For `bench_orders`, the hot path should use explicit interval assignment:

```text
if row_id in hot_tenant_1_status_2_range:
  tenant_id = 1
  status = 2
  channel = choose_from_hot_channel_distribution(row_id)
elif row_id in hot_tenant_1_other_range:
  tenant_id = 1
  status = choose_from_remaining_status_distribution(row_id)
...
```

This is preferred over:

```text
tenant_id = weighted_random(...)
status = weighted_random(...)
```

because the latter only approximates the desired cardinality.

## Parameter Pack Contract

The harness must freeze a named parameter-pack catalog. Named packs are part of
the evidence bundle and must not be silently changed.

Required packs:

- `hot_page`: tenant `1`, status `2`
- `hot_page_fn`: tenant `1`, status `2`, channel `'push'`
- `hot_offset`: tenant `1`, status `2`, offset `10000`
- `hot_offset_fn`: tenant `1`, status `2`, offset `10000`, channel `'push'`
- `events_hot`: tenant `1`
- `events_hot_fn`: tenant `1`, `abs(priority_bucket) = 7`
- `agg_hot`: tenant `1`
- `agg_hot_fn`: tenant `1`, channel `'push'`
- `orm_hot`: hot tenant point lookup pack
- `orm_mixed`: mixed hot/cold tenant lookup pack

Rule:

- each scenario result row must record the parameter-pack id, not only the raw
  values
- raw values must also be recorded for reproducibility

## Harness Run Modes

### `gen`

Responsibilities:

- create benchmark tables
- create benchmark indexes
- load deterministic data
- optionally run `ANALYZE`
- optionally verify cardinality checkpoints

Server preparation policy:

- `gen` is the one-time dataset preparation step on the benchmark server
- after `gen` and `ANALYZE` complete, the dataset should be backed up
- later measured runs should restore that backup instead of rerunning `gen`

Required verification checkpoints:

- row counts by table
- hot `bench_orders` cardinality:
  - `tenant_id = 1 AND status = 2`
  - `tenant_id = 1 AND status = 2 AND lower(channel) = 'push'`
- selected sample row checksums for determinism

Reserved command shape:

```bash
python3 scripts/benchmark_m1_baseline.py gen \
  --dsn postgres://... \
  --scale L \
  --seed 20260412 \
  --engine-label db9_before \
  --analyze
```

PostgreSQL generation uses the same command shape with a PostgreSQL DSN and
`--engine-label postgres_18_3`.

### `run`

Responsibilities:

- load the scenario list
- run warmup iterations
- run measured iterations
- run optional representative `EXPLAIN`
- collect process metrics and engine counters
- write one raw JSON result file

Reserved command shape:

```bash
python3 scripts/benchmark_m1_baseline.py run \
  --dsn postgres://... \
  --scale L \
  --seed 20260412 \
  --engine-label db9_after \
  --scenario-set m2_m4 \
  --warmup 3 \
  --measured 7 \
  --metrics-url http://127.0.0.1:9090/internal/metrics \
  --capture-prom-metric kv_requests=db9_server_kv_requests_total \
  --output benchmarks/raw/db9_after_L.json
```

PostgreSQL run uses the same command shape:

```bash
python3 scripts/benchmark_m1_baseline.py run \
  --dsn postgres://... \
  --scale L \
  --seed 20260412 \
  --engine-label postgres_18_3 \
  --scenario-set m2_m4 \
  --warmup 3 \
  --measured 7 \
  --output benchmarks/raw/postgres_18_3_L.json
```

### `compare`

Responsibilities:

- load the three raw result files
- verify schema compatibility
- verify dataset identity
- compute delta tables
- write one comparison report

Reserved command shape:

```bash
python3 scripts/benchmark_m1_baseline.py compare \
  --before benchmarks/raw/db9_before_L.json \
  --after benchmarks/raw/db9_after_L.json \
  --postgres benchmarks/raw/postgres_18_3_L.json \
  --output benchmarks/reports/m2_m4_L_compare.json
```

## Scenario Sets

Reserved scenario-set names:

- `all`
- `m2`
- `m2_m4`
- `m5`
- `m6`
- `m7`
- `m8`

Expansion rules:

- `m2` => `Q01`, `Q01F`, `Q02`, `Q02F`
- `m2_m4` => `Q01`, `Q01F`, `Q02`, `Q02F`, `Q03`, `Q03F`, `Q04`, `Q04F`,
  `Q05`, `Q05F`
- `m5` => `Q06`, `Q06F`
- `m6` => `Q07`, `Q07F`
- `m7` => `Q08`, `Q08F`
- `m8` => `Q09`, `Q09F`, `Q10`, `Q10F`

## Execution Conventions

### Warmup And Measured Runs

Defaults:

- warmup: `3`
- measured: `7`
- repeats: `1` for large scenarios unless the milestone explicitly needs
  repeated outer loops

### Prepared Versus Text Protocol

The harness must record the execution mode:

- `simple_text`
- `extended_prepared`
- `extended_cursor_stream`

Rules:

- `Q07` must use `extended_prepared`
- `Q01` and `Q02` early-close variants should use a cursor-style or incremental
  fetch mode if available
- the same protocol mode must be used across all three engine variants for a
  given scenario family

### Early-Close Variant

`Q02` must run two sub-modes:

- `consume_all`
- `consume_3_then_close`

Both sub-modes must be visible in the raw output.

### Parallel Variant

`Q08` must run at:

- `1` worker
- `2` workers
- `4` workers
- `8` workers

Worker level must be stored in the raw output.

### Low-Memory Variant

`Q09` and `Q10` must run at two memory levels:

- `64MB`
- `128MB`

The chosen control surface must be recorded in the raw output. If db9 and
PostgreSQL expose different knobs, the harness must record both the logical
limit and the engine-specific setting used to approximate it.

## Cardinality Validation Queries

The `gen` step must execute and record at least these validation queries:

```sql
SELECT COUNT(*) FROM bench_orders;
SELECT COUNT(*) FROM bench_events;
SELECT COUNT(*) FROM bench_users;

SELECT COUNT(*)
FROM bench_orders
WHERE tenant_id = 1 AND status = 2;

SELECT COUNT(*)
FROM bench_orders
WHERE tenant_id = 1 AND status = 2 AND lower(channel) = 'push';
```

Validation tolerance:

- exact counts are preferred
- if the implementation chooses a deterministic arithmetic generator with slight
  variation, the tolerance for the last two counts must be explicitly
  documented and remain within `+/- 1%`

## Output File Layout

Reserved directory layout:

```text
benchmarks/
  raw/
    db9_before_L.json
    db9_after_L.json
    postgres_18_3_L.json
  reports/
    m2_m4_L_compare.json
  side/
    tpcc/
    tpch/
  server/
    datasets/
      canonical_10gb/
      tpcc_10gb/
      tpch_10gb/
    baselines/
      canonical/
      tpcc/
      tpch/
  logs/
    db9_before_L.log
    db9_after_L.log
    postgres_18_3_L.log
```

Raw result file naming:

```text
<engine_label>_<scale>[_<scenario_set>].json
```

Recommended server baseline naming:

```text
benchmarks/server/baselines/<suite>/accepted_<milestone>.json
benchmarks/server/baselines/<suite>/accepted_<milestone>_compare.json
```

## Required Raw Metadata

Each raw result file must include:

- benchmark schema version
- exact engine version string
- git SHA
- timestamp in UTC
- hostname
- kernel or platform string
- harness version
- engine label
- DSN redacted to non-secret form
- scale
- seed
- warmup and measured counts
- scenario-set name
- table counts
- server settings relevant to the run
- Prometheus scrape URL and captured metric spec list when metrics scraping is enabled

## Compare-Stage Rules

The compare stage must fail fast if:

- schema versions differ
- scale differs
- seed differs
- scenario ids differ
- parameter-pack ids differ

The compare stage must compute at least:

- absolute delta
- percentage delta
- whether lower is better or higher is better for the metric
- representative lines for `db9_before`, `db9_after`, and `postgres_18_3`
- one explicit PostgreSQL reference line per scenario family in the merged report

For milestone acceptance on the benchmark server:

- `db9_after` must be compared against the last accepted server baseline of the
  same suite
- the baseline artifact should come from the previous accepted milestone result
- a newer ad hoc branch run must not silently replace the accepted baseline

## CI And Review Expectations

The future harness should be able to support two workflows:

- human-driven local benchmark on the `8c36g` EC2 host
- CI verification on a reduced scenario set or reduced scale

The reduced CI path must preserve:

- scenario ids
- parameter-pack ids
- output schema
- metric field names

It may reduce:

- row count
- measured iterations
- worker levels

## Acceptance For This Contract

This contract is ready for implementation when:

- the generator rules are specific enough to reproduce the hot-path counts
- the parameter packs are frozen
- the command convention is frozen
- the output layout is frozen
- the raw result schema file is frozen

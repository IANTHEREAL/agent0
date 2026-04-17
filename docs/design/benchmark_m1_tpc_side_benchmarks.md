# M1 TPC Side Benchmarks

**Status**: Draft  
**Author**: AI Assistant  
**Date**: 2026-04-12  
**Scope**: How to run `TPC-C` and `TPC-H` style side benchmarks against db9
without replacing the canonical `M1` benchmark corpus

> **Draft / non-SoT note**
>
> This document is a benchmark runbook for side benchmarks. It does not replace
> [`benchmark_m1_baseline.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_baseline.md)
> as the primary acceptance contract.

---

## Goal

Add `TPC-C` and `TPC-H` style side benchmarks to the `M1` benchmark inventory
so later milestones can answer two additional questions:

- does db9 improve on realistic OLTP transaction mixes beyond the canonical
  pagination corpus
- does db9 improve on large analytical query shapes beyond the canonical
  `Q01` to `Q10` families

These side benchmarks are additive evidence only.

## Positioning

`TPC-C` and `TPC-H` are not the main `M1` gate.

Use them like this:

- canonical corpus:
  [`benchmark_m1_baseline.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_baseline.md)
  is the blocking milestone gate
- `TPC-C` is the side benchmark for OLTP throughput, transactional contention,
  and prepared-statement or connection-pool behavior
- `TPC-H` is the side benchmark for scan, join, aggregate, sort, and analytical
  end-to-end behavior

## Existing Repo Asset

[`tests/12_tpcc_basic.sql`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/tests/12_tpcc_basic.sql)
already exists, but it is only a functional `TPC-C`-style SQL smoke test.

It is useful for:

- schema sanity
- join or query-shape portability
- PostgreSQL parity checks on simplified `TPC-C` style SQL

It is not sufficient for:

- throughput measurement
- concurrency measurement
- official or quasi-official `TPC-C` style performance numbers

## Recommended Tools

### Standard Choice: HammerDB Fork

Recommended standard side-benchmark runner for db9 and PostgreSQL `18.3`:

- project: [HammerDB](https://www.hammerdb.com/)
- fork: [dbsid/HammerDB](https://github.com/dbsid/HammerDB)
- why:
  - one runner family for both `TPC-C` and `TPC-H`
  - easier to adapt at the PostgreSQL workload-script layer than `tiup bench`
  - already validated locally against PostgreSQL `18.3`
  - db9 compatibility work can live in one dedicated fork instead of a growing
    pile of shell wrappers

Important note:

- HammerDB uses `TPROC-C` and `TPROC-H` naming for its public benchmark
  reporting flow
- in internal db9 documents we still map these to `TPC-C` / `TPC-H` style side
  benchmarks
- in public reporting, keep HammerDB's `TPROC-*` naming and do not imply
  audited `TPC-C` / `TPC-H` comparability

This fork carries db9-specific PostgreSQL compatibility logic such as:

- db9 detection through `select version()`
- db9-safe role/bootstrap SQL
- db9-safe `TPROC-C` non-stored-procedure profile
- db9 fallback handling for fragile PostgreSQL catalog assumptions

Reference document in the fork:

- [`db9-compat.md`](https://github.com/dbsid/HammerDB/blob/main/docs/db9-compat.md)

### Secondary Choice: BenchBase

BenchBase remains useful as an engineering comparison tool, especially when the
team wants a JDBC-centric runner, but it is no longer the standard `M1`
`TPC-C` / `TPC-H` side-benchmark tool.

### Legacy Fallback: `tiup bench`

`tiup bench` is still useful as a historical local fallback and for ad hoc
comparisons, but it is no longer the standard runner for `M1` side benchmarks.

Keep the existing `tiup bench` wrapper only as a legacy reference:

- [`scripts/run_local_tiup_tpc_compare.sh`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/scripts/run_local_tiup_tpc_compare.sh)

## Recommended db9 Setup

For local side benchmark runs against db9:

- run db9 with a dedicated TiKV or CSE cluster, not a shared development store
- prefer one isolated benchmark database per run
- set `DB9_AUTO_ANALYZE_ENABLED=false` to avoid background analyze noise in
  latency and throughput results
- expose db9 metrics with `DB9_METRICS_PORT=9090`
- ensure Redis is reachable if the current db9 binary still requires
  `REDIS_URL` during startup
- when using keyspaces on TiKV or CSE, ensure the storage side is configured for
  API V2 so db9 keyspace connections do not fail with `ApiVersionNotMatched`

For server-side acceptance runs:

- prepare one fixed `10GB` `TPC-C` dataset artifact on the benchmark server
- prepare one fixed `10GB` `TPC-H` dataset artifact on the benchmark server
- back up each prepared dataset after load and statistics preparation complete
- restore the prepared backup before each measured side benchmark run
- do not regenerate `TPC-C` or `TPC-H` data inside the measured benchmark
  window

Recommended local launch shape:

```bash
REDIS_URL=redis://127.0.0.1:6379/0 \
DB9_DEV=1 \
DB9_DEV_ADMIN_PASSWORD=admin \
DB9_AUTO_ANALYZE_ENABLED=false \
DB9_TENANT_MEMORY_QUOTA_BYTES=0 \
DB9_STATEMENT_TIMEOUT_MS=0 \
DB9_STATEMENT_TIMEOUT_HARD_CAP_MS=0 \
DB9_METRICS_PORT=9090 \
PD_ENDPOINTS=127.0.0.1:<pd_port> \
PG_PORT=5433 \
./target/release/db9-server
```

## How To Run `TPC-C` Against db9

### Suggested Workflow

Use the HammerDB fork against db9's PostgreSQL endpoint.

Standard local runner:

- repo: [dbsid/HammerDB](https://github.com/dbsid/HammerDB)
- benchmark flavor: `TPROC-C`
- recommended db9 mode: `pg_storedprocs=false`

High-level steps:

1. use the HammerDB fork
2. use the db9-specific `TPC-C` non-stored-procedure script set
3. run `buildschema`
4. run correctness checks after `prepare`
5. run the timed workload
6. run correctness checks after `run`
7. capture HammerDB output plus db9 metrics and correctness JSON

Suggested command pattern from the fork:

```bash
git clone https://github.com/dbsid/HammerDB.git
cd HammerDB

./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_db9_nosp_buildschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_db9_nosp_checkschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_db9_nosp_run.tcl
```

Equivalent PostgreSQL `18.3` commands use the `pg18` scripts from the same
fork:

```bash
./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_pg18_buildschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_pg18_checkschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tprocc/pg_tprocc_pg18_run.tcl
```

### Legacy `tiup bench` Commands For Local Compare

The following `tiup bench` commands are kept only as a historical fallback:

Prepare on PostgreSQL 18.3:

```bash
tiup bench tpcc prepare \
  -d postgres \
  -H 127.0.0.1 \
  -P 5432 \
  -U <local_pg_user> \
  -D tpcc10g_pg18 \
  --conn-params sslmode=disable \
  --warehouses 100 \
  --dropdata \
  --no-check
```

Prepare on db9:

```bash
tiup bench tpcc prepare \
  -d postgres \
  -H 127.0.0.1 \
  -P 5433 \
  -U admin \
  -p admin \
  -D tpcc10g_db9 \
  --conn-params sslmode=disable \
  --warehouses 100 \
  --dropdata \
  --no-check
```

Run on PostgreSQL 18.3:

```bash
tiup bench tpcc run \
  -d postgres \
  -H 127.0.0.1 \
  -P 5432 \
  -U <local_pg_user> \
  -D tpcc10g_pg18 \
  --conn-params sslmode=disable \
  --warehouses 100 \
  -T 32 \
  --time 5m \
  --output json
```

Run on db9:

```bash
tiup bench tpcc run \
  -d postgres \
  -H 127.0.0.1 \
  -P 5433 \
  -U admin \
  -p admin \
  -D tpcc10g_db9 \
  --conn-params sslmode=disable \
  --warehouses 100 \
  -T 32 \
  --time 5m \
  --output json
```

Important local note:

- on db9, prefer `DB9_TENANT_MEMORY_QUOTA_BYTES=0` and
  `DB9_STATEMENT_TIMEOUT_MS=0` during this local `TPC-C` compare, otherwise
  `prepare` may fail on memory quota or statement timeout before the run begins

### TPC-C Correctness Validation

`TPC-C` side benchmarks should not stop at throughput alone. For db9 milestone
work, the local and server-side flow should validate relational invariants both
after `prepare` and after `run`.

This repo now includes the authoritative post-benchmark correctness checker:

- [`scripts/tpcc_correctness_check.py`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/scripts/tpcc_correctness_check.py)

The correctness checker validates invariants such as:

- each warehouse owns exactly `10` districts
- each district owns exactly `3000` customers
- each district's `d_next_o_id` matches both `COUNT(orders)` and `MAX(o_id) + 1`
- every `new_order` row points at an `orders` row with `o_carrier_id IS NULL`
- every order with `o_carrier_id IS NULL` still has a `new_order` row
- every order has `5..15` `order_line` rows and matches `orders.o_ol_cnt`
- `history`, `orders`, and `order_line` rows still reference valid parents

Example direct commands:

Validate PostgreSQL 18.3 after `prepare`:

```bash
python3 scripts/tpcc_correctness_check.py \
  --host 127.0.0.1 \
  --port 5432 \
  --user <local_pg_user> \
  --db tpcc10g_pg18 \
  --label pg18 \
  --phase after_prepare \
  --output /tmp/tpcc_pg18_after_prepare_correctness.json
```

Validate db9 after `run`:

```bash
python3 scripts/tpcc_correctness_check.py \
  --host 127.0.0.1 \
  --port 5433 \
  --user admin \
  --password admin \
  --db tpcc10g_db9 \
  --label db9 \
  --phase after_run \
  --output /tmp/tpcc_db9_after_run_correctness.json
```

Standard correctness flow:

1. HammerDB `buildschema`
2. HammerDB `checkschema`
3. custom `tpcc_correctness_check.py --phase after_prepare`
4. HammerDB `run`
5. custom `tpcc_correctness_check.py --phase after_run`

Correctness acceptance rule:

- PostgreSQL `18.3` and db9 must both pass the post-run correctness check for
  the benchmark result to count as valid
- HammerDB `checkschema` is useful, but the invariant checker is the
  authoritative post-run correctness gate

Current local validation status:

- PostgreSQL `18.3`:
  - HammerDB `TPROC-C buildschema/checkschema/run` works locally
  - post-run invariant checking is already supported by
    `tpcc_correctness_check.py`
- db9:
  - HammerDB `TPROC-C buildschema` works locally in `pg_storedprocs=false`
    mode
  - remaining db9 gaps are now later-stage HammerDB runtime and check logic
    issues, not the original bootstrap blocker set

### Suggested Scale Levels For db9

For the current `M1` inventory, treat these as practical internal levels:

- smoke: `1` warehouse, `4` terminals, `5` minutes
- daily side benchmark: `10` warehouses, `16` terminals, `10` to `15` minutes
- milestone side benchmark: `25` to `50` warehouses, `32` terminals, `20` to
  `30` minutes

What `TPC-C` is best at surfacing for db9:

- pgwire protocol overhead
- prepared statement and transaction path stability
- pooled client traffic
- contention and retry behavior
- later `M6` plan-cache improvements

## How To Run `TPC-H` Against db9

### Suggested Workflow

Use the same HammerDB fork as the standard runner.

Suggested command pattern:

```bash
git clone https://github.com/dbsid/HammerDB.git
cd HammerDB

./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_db9_buildschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_db9_checkschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_db9_run.tcl
```

Equivalent PostgreSQL `18.3` commands:

```bash
./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_pg18_buildschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_pg18_checkschema.tcl
./hammerdbcli auto scripts/tcl/postgres/tproch/pg_tproch_pg18_run.tcl
```

### Legacy `tiup bench` Commands For Local Compare

For the requested local compare flow:

- db9-server: PR `#2402`
- cloud-storage-engine: PR `#4921`
- PostgreSQL: `18.3`

The practical `tiup bench` commands are:

Prepare on PostgreSQL 18.3:

```bash
tiup bench tpch prepare \
  -d postgres \
  -H 127.0.0.1 \
  -P 5432 \
  -U <local_pg_user> \
  -D tpch10g_pg18 \
  --conn-params sslmode=disable \
  --sf 10 \
  --dropdata \
  --analyze
```

Prepare on db9:

```bash
tiup bench tpch prepare \
  -d postgres \
  -H 127.0.0.1 \
  -P 5433 \
  -U admin \
  -p admin \
  -D tpch10g_db9 \
  --conn-params sslmode=disable \
  --sf 10 \
  --dropdata \
  --analyze
```

Run on PostgreSQL 18.3:

```bash
tiup bench tpch run \
  -d postgres \
  -H 127.0.0.1 \
  -P 5432 \
  -U <local_pg_user> \
  -D tpch10g_pg18 \
  --conn-params sslmode=disable \
  --sf 10 \
  --time 5m \
  --output json
```

Run on db9:

```bash
tiup bench tpch run \
  -d postgres \
  -H 127.0.0.1 \
  -P 5433 \
  -U admin \
  -p admin \
  -D tpch10g_db9 \
  --conn-params sslmode=disable \
  --sf 10 \
  --time 5m \
  --output json
```

Important local note:

- on db9, prefer `DB9_STATEMENT_TIMEOUT_MS=0` during this local `TPC-H`
  compare, otherwise long analytical queries may time out before the full suite
  completes

### Suggested Scale Levels For db9

For internal engineering:

- smoke: scale factor `0.1`
- daily side benchmark: scale factor `1`
- milestone side benchmark: scale factor `3` to `10`, depending on local load
  time budget and disk budget

### TPC-H Correctness Validation

`TPC-H` is read-only during `run`, so the standard correctness rule is:

1. HammerDB `buildschema`
2. HammerDB `checkschema`
3. HammerDB `run`
4. HammerDB `checkschema` again

For db9 and PostgreSQL `18.3`:

- if `checkschema` passes before and after `run`, the benchmark result is
  treated as correctness-valid
- this is sufficient for `TPC-H` because the workload does not mutate the
  dataset

### Recommended Query Subset

When a full `TPC-H` run is too expensive for every milestone, prefer a stable
subset that maps well to db9 optimization goals:

- `Q1`: wide scan plus aggregate
- `Q3`: join plus order plus limit
- `Q5`: multi-join aggregate
- `Q9`: join plus expression plus aggregate
- `Q18`: large aggregate plus order plus limit

Use cases by milestone:

- `M5`: `Q1`, `Q9`, `Q18`
- `M7`: `Q1`, `Q5`, `Q9`, `Q18`
- `M8`: `Q1`, `Q3`, `Q18` under constrained memory

## Result Collection

Store side benchmark results separately from the canonical `Q01` to `Q10` raw
files.

Recommended layout:

```text
benchmarks/
  side/
    tpcc/
      db9_after_S.json
      postgres_18_3_S.json
    tpch/
      db9_after_sf1.json
      postgres_18_3_sf1.json
  server/
    datasets/
      tpcc_10gb/
      tpch_10gb/
    baselines/
      tpcc/
      tpch/
```

Minimum fields to preserve:

- tool name and version
- workload name
- db9 commit SHA
- comparison engine and version
- scale factor or warehouse count
- terminals or clients
- warmup and measured duration
- throughput metric from the external tool
- p50 and p95 latency when the tool exposes them
- db9 Prometheus deltas for any captured metrics
- server baseline id when the run participates in milestone acceptance

## How These Side Benchmarks Should Be Used

Recommended usage:

- `TPC-C`: nightly or milestone-side evidence for end-to-end OLTP behavior
- `TPC-H`: nightly or milestone-side evidence for large analytical behavior
- on the benchmark server, compare each new milestone run against the previous
  accepted side-benchmark baseline of the same suite

Do not use them as:

- the only acceptance gate for `M2` to `M8`
- proof that an index-only or ordered-path optimization worked
- proof that a function pushdown compatibility regression did not happen

Those questions must still be answered by the canonical `M1` corpus.

## Publication And Naming Caution

Internal engineering runs are fine.

Public claims need more care:

- do not present non-audited results as official `TPC-C` or `TPC-H` benchmark
  numbers
- do not claim audited comparability from a simple BenchBase or HammerDB run
- if HammerDB is used for public reporting, preserve its `TPROC-C` or `TPROC-H`
  naming

For this repo, the intended use is internal engineering and regression control,
not audited publication.

## Recommendation

For this PR and the `M1` benchmark program:

- keep the current canonical corpus as the blocking gate
- add `TPC-C` and `TPC-H` as side benchmark inventory
- use the HammerDB fork at [dbsid/HammerDB](https://github.com/dbsid/HammerDB)
  as the standard `TPC-C` / `TPC-H` side-benchmark runner
- keep `BenchBase` and `tiup bench` as secondary or legacy fallback tools only
- keep all side benchmark outputs separate from the canonical raw result files

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

### Default Choice: BenchBase

Recommended default tool for db9:

- project: [BenchBase](https://github.com/cmu-db/benchbase)
- why:
  - open source
  - speaks PostgreSQL through JDBC
  - supports both `TPC-C` and `TPC-H`
  - easy to aim at db9 because db9 already exposes pgwire

BenchBase PostgreSQL sample configs exist upstream:

- `config/postgres/sample_tpcc_config.xml`
- `config/postgres/sample_tpch_config.xml`

This makes it the most practical default for internal engineering runs.

### Optional Alternative: HammerDB

Optional alternative when a team wants a single packaged benchmark runner with
its own scripting flow:

- project: [HammerDB](https://www.hammerdb.com/)

Important note:

- HammerDB uses `TPROC-C` and `TPROC-H` naming for its public benchmark
  reporting flow
- if we use HammerDB-derived workloads, we should preserve that naming in public
  reports and avoid implying audited `TPC-C` or `TPC-H` comparability

For this repo, BenchBase is still the recommended first choice because it is
lighter to integrate with db9's PostgreSQL surface.

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

Recommended local launch shape:

```bash
REDIS_URL=redis://127.0.0.1:6379/0 \
DB9_DEV=1 \
DB9_DEV_ADMIN_PASSWORD=admin \
DB9_AUTO_ANALYZE_ENABLED=false \
DB9_METRICS_PORT=9090 \
PD_ENDPOINTS=127.0.0.1:<pd_port> \
PG_PORT=5433 \
./target/release/db9-server
```

## How To Run `TPC-C` Against db9

### Suggested Workflow

Use BenchBase against db9's PostgreSQL endpoint.

Connection mapping:

- JDBC URL:
  `jdbc:postgresql://127.0.0.1:5433/postgres?sslmode=disable&ApplicationName=tpcc`
- username: `admin`
- password: `admin`

High-level steps:

1. build BenchBase
2. copy the upstream PostgreSQL `TPC-C` sample config
3. replace the host, port, username, password, and database with db9 values
4. run create
5. run load
6. run execute
7. capture both BenchBase output and db9 Prometheus deltas

Suggested command pattern:

```bash
git clone https://github.com/cmu-db/benchbase.git
cd benchbase
./mvnw clean package -DskipTests

java -jar target/benchbase-postgres.jar \
  -b tpcc \
  -c config/postgres/sample_tpcc_config.xml \
  --create=true --load=true --execute=true
```

The exact jar name can vary by BenchBase build layout. The key point is to use
the PostgreSQL build target and the upstream `sample_tpcc_config.xml`.

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

Use BenchBase first, because it keeps the runner family consistent with the
`TPC-C` side benchmark and already ships a PostgreSQL `TPC-H` sample config.

Connection mapping:

- JDBC URL:
  `jdbc:postgresql://127.0.0.1:5433/postgres?sslmode=disable&ApplicationName=tpch`
- username: `admin`
- password: `admin`

Suggested command pattern:

```bash
git clone https://github.com/cmu-db/benchbase.git
cd benchbase
./mvnw clean package -DskipTests

java -jar target/benchbase-postgres.jar \
  -b tpch \
  -c config/postgres/sample_tpch_config.xml \
  --create=true --load=true --execute=true
```

### Suggested Scale Levels For db9

For internal engineering:

- smoke: scale factor `0.1`
- daily side benchmark: scale factor `1`
- milestone side benchmark: scale factor `3` to `10`, depending on local load
  time budget and disk budget

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

## How These Side Benchmarks Should Be Used

Recommended usage:

- `TPC-C`: nightly or milestone-side evidence for end-to-end OLTP behavior
- `TPC-H`: nightly or milestone-side evidence for large analytical behavior

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
- prefer BenchBase as the default runner for both because it fits db9's
  PostgreSQL protocol surface
- keep all side benchmark outputs separate from the canonical raw result files

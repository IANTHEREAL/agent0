# Performance Optimization Parameters

## Purpose

This document tracks all parameters introduced by the `M1` to `M7` performance optimization program.

It exists for three purposes:

- explain the scope of each new parameter
- explain what the parameter changes and when it should be used
- give operators and developers one place to review rollout, tuning, and rollback knobs introduced by the optimization work

## Relationship To Other Docs

- This document is the detailed usage guide for optimization-related parameters only.
- The authoritative registry for operator-facing `db9-server` runtime inputs remains [ops-config](./sot/ops-config.md).
- If a change introduces a new `db9-server` environment variable, CLI flag, or other operator-facing runtime input, the same change must update both this document and [ops-config](./sot/ops-config.md).
- This document may also describe optimization-related knobs that live in `cloud-storage-engine`, as long as they are part of the DB9-scoped optimization path and are clearly labeled as such.

## Update Rule

Every optimization milestone that introduces any of the following must update this document in the same PR or PR series:

- `db9-server` environment variables
- `db9-server` CLI flags
- session or system GUCs
- statement-level tuning flags or hints
- optimizer enable or disable switches
- protocol capability flags that are operator-visible
- `cloud-storage-engine` runtime knobs used by DB9-scoped optimization
- DDL options for indexes or storage layout that change optimization behavior

If a milestone introduces no new parameter, the milestone close note should explicitly state `no new optimization parameter introduced`.

## Scope Definitions

Use one of the following values in the parameter table:

| Scope | Meaning | Typical Lifetime | Example Use |
|---|---|---|---|
| `server-startup` | `db9-server` process startup input such as env var or CLI flag | process lifetime | enable a rollout flag cluster-wide |
| `session-guc` | session-level SQL setting | session lifetime | enable or disable a planner path for one client session |
| `statement` | statement-level tuning control | single statement | force a benchmark or debugging path |
| `ddl-index` | schema or index definition option | schema lifetime | enable a covering-index payload option |
| `protocol-capability` | negotiated client or remote capability | connection or request lifetime | enable streaming or ordered remote execution |
| `cse-runtime` | `cloud-storage-engine` DB9-scoped runtime knob | process or request lifetime | limit remote page size for DB9 streaming |
| `debug-experiment` | non-default debugging or experiment-only control | temporary | isolate rollout or failure analysis |

## Required Fields For Every Parameter

Every parameter entry must include:

- parameter name
- owning repo
- scope
- default value
- allowed values or range
- who can set it
- whether it is safe for production
- exact behavior it changes
- rollout recommendation
- rollback method
- usage examples
- observability hooks: metric, log, or `EXPLAIN` signal that proves it is active

## Parameter Inventory

Add new entries to this table as optimization work lands.

| Parameter | Repo | Scope | Default | Allowed Values | Production Safe | Summary |
|---|---|---|---|---|---|---|
| `None yet` | n/a | n/a | n/a | n/a | n/a | Optimization parameter inventory starts when `M1` introduces the first real knob. |

## Detailed Entries

Use the following template for every new parameter.

### `<parameter-name>`

- `Milestone`: `M1` to `M7`
- `Repo`: `db9-server` or `cloud-storage-engine`
- `Scope`: one of the scope values defined above
- `Default`: `<value>`
- `Allowed Values`: `<range or enum>`
- `Set By`: `operator`, `session`, `benchmark harness`, `internal capability negotiation`, or similar
- `Production Safe`: `yes`, `guarded rollout`, or `debug only`
- `Summary`: one sentence
- `Why It Exists`: explain which bottleneck it addresses
- `Behavior`: explain exactly what execution path, planner choice, buffering rule, payload rule, or remote behavior changes
- `When To Use`: explain the recommended scenario
- `When Not To Use`: explain the unsafe or unhelpful scenarios
- `Observability`: list metrics, logs, `EXPLAIN` output, or counters that prove the parameter is active
- `Rollback`: exact action to disable or revert

Example usage:

```sql
-- session-level example
SET some.optimization_flag = 'on';
```

```bash
# server-startup example
SOME_OPTIMIZATION_FLAG=1 ./target/release/db9-server
```

## Milestone Checklist

Before closing any optimization milestone:

- update this document if a new parameter was introduced
- update [ops-config](./sot/ops-config.md) if the parameter is an operator-facing `db9-server` runtime input
- verify the usage examples actually work
- verify the documented default matches code
- verify the documented scope matches how the parameter is read and applied
- attach the parameter doc diff in the milestone evidence bundle

## Notes For `M5` Shared Plan Cache

- Prefer PostgreSQL-compatible control semantics through `plan_cache_mode=auto|force_custom_plan|force_generic_plan`.
- If `db9-server` adds DB9-specific resource governance for shared plan cache, keep it separate from PostgreSQL compatibility controls.
- The currently planned DB9-specific resource parameter is `db9.shared_plan_cache_max_bytes`.
- Do not reintroduce a DB9-only TTL parameter unless a later design review reopens that decision explicitly.

## Notes For `cloud-storage-engine`

- Any parameter documented here for `cloud-storage-engine` must remain strictly inside the DB9-scoped optimization path.
- It must not alter existing TiDB to TiKV behavior or compatibility outside the DB9 extension surface.
- If a `cloud-storage-engine` parameter is request-scoped or capability-scoped rather than operator-settable, document that explicitly so it is not mistaken for a supported TiDB or TiKV control surface.

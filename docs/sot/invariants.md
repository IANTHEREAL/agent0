# System Invariants (Cross-Module)

## Scope

System-wide invariants that span multiple modules. These are binding rules that MUST hold at all times across the entire db9-server codebase.

## Invariants

1. **Single execution path**: There MUST be no hidden fallback. If the Analyzer succeeds, execution MUST use typed expressions only.
   - xref: [sql-engine](./sql-engine.md)

2. **TypedExpr completeness**: Every expression node MUST carry a resolved `DataType`. No unresolved names MUST escape the Analyzer.
   - xref: [sql-engine](./sql-engine.md)

3. **EXPLAIN = execution**: EXPLAIN MUST use the same index selection logic as runtime. They MUST NOT diverge.
   - xref: [sql-engine](./sql-engine.md)

4. **Keyspace isolation**: All persistent data MUST be scoped to the tenant's keyspace. No cross-tenant data access MUST be possible.
   - xref: [storage-format](./storage-format.md), [multi-tenancy](./multi-tenancy.md)

5. **View transparency**: Views MUST be expanded before the Analyzer runs. The rest of the pipeline MUST NOT see view references.
   - xref: [sql-engine](./sql-engine.md)

6. **CTE materialization before main query**: All CTEs in a WITH clause MUST be fully executed and materialized before the main query begins.
   - xref: [sql-engine](./sql-engine.md)

7. **Privilege check before scan**: SELECT privilege MUST be enforced before any table data is read.
   - xref: [auth-rbac](./auth-rbac.md)

8. **Autocommit retry is safe**: Only autocommit (implicit) transactions MUST be retried. Explicit transactions MUST NOT retry automatically.
   - xref: [storage-format](./storage-format.md)

9. **Trigger ordering**: BEFORE triggers MUST execute inline (blocking). AFTER triggers MUST execute asynchronously after commit.
   - xref: [sql-engine](./sql-engine.md), [worker-cron](./worker-cron.md)

10. **Test expectations match PostgreSQL**: No `.expected` file MUST be updated without verifying against real PostgreSQL 17.7 output.
    - xref: [testing-gates](./testing-gates.md)

11. **Worker queue isolation**: Worker tasks MUST be scoped to `_sys_worker` keyspace. Task claiming MUST use pessimistic transactions — no duplicate execution across instances.
    - xref: [worker-cron](./worker-cron.md)

12. **No implicit PostgreSQL divergence**: SQL-visible behavior that intentionally differs from PostgreSQL MUST be explicitly declared and governed in SoT; hidden divergence is not allowed.
    - xref: [sql-engine](./sql-engine.md), [testing-gates](./testing-gates.md)

## Stability

**Stable** — Breaking changes to any invariant require DR/ADR + migration + rollback + gate updates.

## Verification

- Invariants #1-3, #5-6: enforced by the Analyzer → Optimizer → Executor pipeline (`src/sql/analyzer/`, `src/sql/optimizer/`, `src/sql/executor/`)
- Invariant #4: enforced by `TikvStore` keyspace prefix and `TikvClientPool` (`src/storage/tikv_store/`, `src/pool.rs`)
- Invariant #7: enforced by `require_table_privilege(Select)` in `src/sql/executor/core/statement.rs`
- Invariant #8: enforced by retry logic in `src/sql/executor/core/mod.rs` (autocommit only)
- Invariant #9: enforced by `src/sql/triggers/` (BEFORE inline) and `src/worker/` (AFTER async)
- Invariant #10: enforced by SQL test contract and PR review policy
- Invariant #11: enforced by `src/worker/engine.rs` (pessimistic transaction claiming)
- Invariant #12: enforced by SoT compatibility strategy + SQL test annotation/evidence rules (`docs/sot/README.md`, `docs/sot/sql-engine.md`, `docs/sot/testing-gates.md`)

## Change Management

Any change to these invariants MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`. Breaking changes require DR/ADR per #368 rules.

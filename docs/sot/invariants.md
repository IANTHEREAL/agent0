# System Invariants (Cross-Module)

## Scope

System-wide invariants that span multiple modules. These are binding rules that must hold across the db9-server codebase.

## Invariants

1. **Analyzed execution stays on one semantic path**
   - Once a statement enters analyzed query execution, analyzed DML, or analyzed prepared execution, the engine MUST NOT silently switch to an alternate planner/executor because analysis failed.
   - Explicit compatibility exceptions at the protocol/parser boundary and documented prepared reparse paths are allowed.
   - xref: [sql-engine](./sql-engine.md)

2. **TypedExpr completeness**
   - Every analyzed expression node MUST carry a resolved `DataType`. No unresolved names may escape the analyzer.
   - xref: [sql-engine](./sql-engine.md)

3. **EXPLAIN and execution share the same semantic pipeline**
   - `EXPLAIN SELECT/WITH` MUST use the same analyze/rewrite and access-path logic as runtime execution.
   - xref: [sql-engine](./sql-engine.md)

4. **Keyspace isolation**
   - All persistent data MUST be scoped to the tenant keyspace. No cross-tenant persistent access is allowed.
   - xref: [storage-format](./storage-format.md), [multi-tenancy](./multi-tenancy.md)

5. **View transparency before analysis**
   - Views MUST be expanded before semantic analysis so the rest of the analyzed pipeline operates on expanded query trees.
   - xref: [sql-engine](./sql-engine.md)

6. **CTE materialization contract**
   - CTE materialization behavior MUST remain explicit and deterministic for the supported execution path; callers must not depend on undocumented hidden rewrites.
   - xref: [sql-engine](./sql-engine.md)

7. **Privilege check before table read**
   - Supported executor privilege checks MUST happen before reading protected table data.
   - xref: [auth-rbac](./auth-rbac.md)

8. **Retry safety boundary**
   - Automatic retries are allowed only when restarting the statement is semantically safe:
     - autocommit statements, or
     - the first statement of an explicit transaction before prior successful statements exist in that transaction.
   - xref: [sql-engine](./sql-engine.md)

9. **Trigger ordering**
   - BEFORE triggers MUST execute inline. AFTER triggers MUST be enqueued for asynchronous post-commit execution.
   - xref: [sql-engine](./sql-engine.md), [worker-cron](./worker-cron.md)

10. **PostgreSQL evidence before changing SQL expectations**
    - No `.expected`, `.errors`, or `.assert` contract for SQL-visible behavior may be changed without PostgreSQL 17.7 verification.
    - xref: [testing-gates](./testing-gates.md)

11. **Worker task single-winner semantics**
    - Worker task claiming MUST remain single-winner across instances, using the documented storage/transaction semantics for task coordination.
    - xref: [worker-cron](./worker-cron.md), [storage-format](./storage-format.md)

12. **No implicit PostgreSQL divergence**
    - SQL-visible behavior that intentionally differs from PostgreSQL MUST be explicitly declared and governed in SoT. Hidden divergence is not allowed.
    - xref: [sql-engine](./sql-engine.md), [testing-gates](./testing-gates.md), [README](./README.md)

## Stability

**Stable**. Breaking changes to any invariant require DR/ADR, migration, rollback, and gate updates.

## Verification

- Invariants #1-3 and #5-6: analyzer/rewrite/optimizer/executor pipeline
- Invariant #4: TiKV keyspace isolation and tenant routing
- Invariant #7: executor privilege helper callsites
- Invariant #8: transaction retry framework
- Invariant #9: trigger enqueue/worker execution split
- Invariant #10: SQL test harness contract and parity evidence rules
- Invariant #11: worker claiming and system-keyspace coordination
- Invariant #12: SoT compatibility strategy and divergence governance

## Change Management

Any change to these invariants MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`. Breaking changes require DR/ADR per #368 rules.

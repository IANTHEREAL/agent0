# Documentation Drift Adjudication

**Status**: Draft  
**Date**: 2026-03-06  
**Source**: issue `#1514` and comment `#issuecomment-4010303875`

## Purpose

This document adjudicates the disputed findings in the documentation drift audit against the current repository state. It answers three questions for each finding:

1. Is the finding factually valid on the current codebase?
2. What is the broken layer: SoT, architecture narrative, roadmap wording, or implementation?
3. What is the right fix under db9 principles:
   - PostgreSQL-compatible by default
   - best distributed PostgreSQL-compatible database, not blind parity theater
   - intentional divergence is allowed only when explicitly documented

## Decision Rules

- If implementation is already correct and the docs are stale, fix the docs at the authoritative layer.
- If the docs promise something broader than the implementation intentionally guarantees, narrow the contract instead of deleting useful compatibility behavior.
- If a shipped feature exists but the roadmap still tracks a broader future phase, split "current shipped state" from "remaining roadmap work".
- `docs/design/**` is not SoT. Historical design docs are allowed, but they must be clearly marked as `Draft`, `Historical`, or `Superseded` so they are not mistaken for current architecture.

## Adjudication

1. **RBAC enforcement contradiction**
   - Verdict: Valid.
   - Root layer: `docs/sot/auth-rbac.md` and `docs/sot/modules.yaml` are stale.
   - Facts: `require_privilege()` and `require_table_privilege()` call `AuthManager::check_privilege()` today; SELECT privilege checks are wired through `analyze_then_rewrite_query()`.
   - Decision: Update SoT to say privilege enforcement is wired for the currently supported enforcement points. Keep the "coverage is partial" caveat, but remove any claim that there are no `check_privilege()` call sites.

2. **GIN access path contradiction**
   - Verdict: Valid.
   - Root layer: duplicated, conflicting documentation across SoT and architecture docs.
   - Facts: planner access-path selection can emit `ScanType::GinIndexScan`, optimizer build constructs `GinScanOperator`, and runtime operator support exists.
   - Decision: Make `docs/sot/extensions-gin.md` the authoritative GIN behavior contract. In `docs/sot/sql-engine.md` and `docs/architecture/sql-engine.md`, replace detailed GIN state claims with a short cross-link to the extension SoT.

3. **Top-level SELECT pipeline is stale**
   - Verdict: Valid.
   - Root layer: `docs/ARCHITECTURE.md`.
   - Facts: the real entrypoint is `analyze_then_rewrite_query()`, which includes view expansion, catalog snapshot build, privilege checks, analyzer, and post-analysis rewriter. The documented `QueryPlan` stage and `executor/select/analyzed/query_plan.rs` do not exist.
   - Decision: Rewrite the top-level SELECT pipeline to match the actual code path and remove the retired `QueryPlan` box entirely.

4. **Global "no hidden fallback" invariant is too broad**
   - Verdict: Valid, but the right fix is contract narrowing, not immediate code removal.
   - Root layer: `docs/ARCHITECTURE.md` and `docs/sot/invariants.md`.
   - Facts: current code has explicit fallbacks for raw-SQL utility acceptance at the protocol/parser boundary, prepared recursive CTE execution via text reparse, and prepared schema-drift recovery via text reparse.
   - Historical evidence:
     - raw-SQL parser-boundary fallback was intentionally centralized in `#759`, preserved through parameter-IR integration in PR `#888`, and explicitly kept during parse dedup in PR `#1362`;
     - prepared schema-drift fallback is part of the prepared plan-cache design introduced in PR `#1078`;
     - prepared recursive CTE fallback currently exists because recursive CTE execution over frozen analyzed IR is still not implemented.
   - PostgreSQL verification (2026-03-06, PostgreSQL 17.9):
     - prepared recursive CTEs execute successfully;
     - harmless schema drift (`ALTER TABLE ... ADD COLUMN`) still allows the next `EXECUTE`;
     - incompatible drift (`ALTER TABLE ... RENAME COLUMN`) fails on the next `EXECUTE` with a reparse-time semantic error (`column "a" does not exist`).
   - Decision: Narrow the invariant to the analyzed execution contract:
     - once a statement enters analyzed SELECT / analyzed DML / analyzed prepared execution, the engine must not silently route to an alternate planner or legacy executor because analysis failed;
     - explicit parse-boundary compatibility shims and prepared invalidation/reparse paths are allowed, but must be documented as exceptions.
   - Decision detail:
     - keep parser-boundary `RawSqlUtility` acceptance as an explicit compatibility boundary, not a hidden fallback;
     - keep prepared schema-drift reparse as the correctness mechanism for invalidated frozen plans;
     - treat prepared recursive CTE text reparse as implementation debt rather than a desired long-term design, and track it separately in `#1516`.
   - Rationale: removing these fallbacks now would reduce PG surface compatibility or correctness more than it would improve architecture purity.

5. **`ops-config.md` is not the real runtime config registry**
   - Verdict: Valid.
   - Root layer: `docs/sot/ops-config.md`.
   - Facts: the code reads additional runtime knobs from `src/config.rs`, `src/worker/config.rs`, `src/cron/config.rs`, `src/storage/backpressure.rs`, `src/main.rs`, and `src/cli.rs`. The doc also incorrectly says no CLI flags exist.
   - Decision: Expand `ops-config.md` into the actual operator-facing runtime registry:
     - environment variables
     - supported CLI flags
     - supported fs9 WebSocket knobs
     - supported TiKV backpressure knobs
   - Decision detail: explicitly exclude incidental platform inputs such as `HOSTNAME` unless they are intended as supported operator knobs.

6. **`testing-gates.md` and `modules.yaml` point at deleted workflow files**
   - Verdict: Valid.
   - Root layer: `docs/sot/testing-gates.md`, `docs/sot/modules.yaml`, and the SoT README map.
   - Facts: the repo currently contains `ci.yml`, `doc-lint.yml`, and `governance-lint.yml`; the referenced `orm-tests.yml`, `regression-gate.yml`, `gorm-smoke.yml`, and `sqlalchemy-smoke.yml` files do not exist.
   - Decision: Rewrite the gate registry to the current workflow/job inventory.
   - Design note: do not claim "required merge gate" status unless the source of truth is explicit. Workflow jobs are observable from the repo; GitHub required-check settings are not guaranteed to be inferable from the repo itself.

7. **`storage-format.md` has factual errors and is over-scoped**
   - Verdict: Valid.
   - Root layer: `docs/sot/storage-format.md`.
   - Facts:
     - `TableSchema` is serialized as MessagePack with `DB9_SCHEMA_V2`, not bincode.
     - `Row` is serialized with bincode.
     - `create_keyspace` is not a current entrypoint.
     - the implementation exposes many more metadata key families than the doc currently names.
   - Decision: Fix the incorrect facts immediately. For key coverage, explicitly scope the human SoT to "high-level key families and invariants" and point to the encoding registry/modules as the exhaustive implementation inventory.
   - Rationale: trying to mirror every encoder in a prose SoT page is high-drift and low-signal.

8. **HNSW is shipped but lacks authoritative coverage; design doc describes removed architecture**
   - Verdict: Valid.
   - Root layer: missing SoT/architecture coverage plus a stale design doc.
   - Facts: HNSW runtime support exists, `hnsw.ef_search` exists, scan operator exists, and the current storage model is delta-log plus background sweep/merge. The old process-level graph cache was intentionally removed.
   - Decision: add HNSW to SoT and architecture as a first-class shipped feature. Rewrite the HNSW design doc to the current no-cache, delta-log architecture instead of keeping the wrong "implemented" design text.
   - Rationale: this is a real architectural choice for a distributed system and deserves an authoritative explanation.

9. **Plan cache is shipped, but roadmap still says "in progress"**
   - Verdict: Partially valid; the current roadmap wording is too coarse.
   - Root layer: `docs/ARCHITECTURE.md`.
   - Facts: session-local prepared plan caching, invalidation, promotion thresholds, and GUC wiring are implemented. The repo-level roadmap still tracks broader "Phase 4: plan cache / parameterized plan reuse".
   - Decision: split the story:
     - Completed: session-local prepared plan cache for prepared statements.
     - Remaining roadmap: broader parameterized/shared reuse work tracked by `#707`.
   - Rationale: this preserves truth without pretending the broader milestone is fully done.

10. **`sql-engine.md` has additional factual drift**
   - Verdict: Valid.
   - Root layer: `docs/sot/sql-engine.md`.
   - Facts:
     - `db9.retry_max_attempts` default is `64`, not `10`.
     - the error variant is `InFailedTransaction`, not `InFailedSqlTransaction`.
     - the canonical engine path omits the post-analysis rewriter.
   - Decision: correct the factual values and names; add the rewriter as a real pipeline phase; remove redundant GIN detail in favor of the extension SoT.

11. **`protocol-pgwire.md` uses stale entrypoint paths**
   - Verdict: Valid.
   - Root layer: `docs/sot/protocol-pgwire.md`.
   - Facts: `src/protocol/handler/dynamic.rs` no longer exists as a flat file, and `query_parser.rs` is a real protocol entrypoint that the SoT does not mention.
   - Decision: update protocol SoT entrypoints to the current module split and include `query_parser.rs`.

12. **`catalog-introspection.md` is materially under-scoped**
   - Verdict: Valid, but the right fix is clearer scoping, not a giant table dump.
   - Root layer: `docs/sot/catalog-introspection.md`.
   - Facts: `CatalogRegistry` registers many more virtual tables than the minimum matrix suggests, and `virtual_tables.rs` is core infrastructure.
   - Decision: keep the doc as a minimum matrix if desired, but explicitly point to `CatalogRegistry` and `virtual_tables.rs` as the authoritative registry/mechanism for current coverage.

13. **`multi-tenancy.md` under-documents per-tenant in-memory isolation**
   - Verdict: Valid.
   - Root layer: `docs/sot/multi-tenancy.md`.
   - Facts: `TenantEntry` isolates `TriggerBodyCache` in addition to `TableStatsCache`.
   - Decision: update the multi-tenancy SoT to include trigger cache isolation. Optionally note that other in-memory tenant-scoped state also lives under `TenantEntry`, but the minimum correction is to include `TriggerBodyCache`.

14. **Design-doc drift is systemic**
   - Verdict: Valid.
   - Root layer: documentation process, not a single page.
   - Facts: a large fraction of `docs/design/**` still points at deleted paths, but these pages are not currently labeled as historical or superseded.
   - Decision: make this a structural fix:
     - every design doc must carry a status banner (`Draft`, `Active`, `Historical`, `Superseded`);
     - doc lint should enforce current-path validity only for active docs, not archived historical ones;
     - active design docs with deleted `src/**` references should fail lint;
     - draft docs keep status-only enforcement for now, because they may intentionally describe future target modules instead of shipped paths.
   - Rationale: this fixes the root cause instead of manually chasing path drift forever.

15. **Specific design docs that contradict current architecture**
   - `15a` Worker binary spec
     - Verdict: Valid.
     - Decision: mark `docs/design/24_worker_binary_spec.md` as `Superseded`; current worker execution lives inside the main server process.
   - `15b` Older CBO phase plan with `db9.use_optimizer` gating
     - Verdict: Valid as a historical-plan drift issue.
     - Decision: mark `docs/design/cbo-phase3-pr1-plan.md` as `Historical` or `Superseded`; it is not a current architecture document.
  - `15c` Sequence design scope vs implementation
     - Verdict: Partially valid.
     - Facts: generic `ALTER SEQUENCE` remains unsupported while `ALTER SEQUENCE ... OWNED BY` exists. The document is still a draft design, not SoT.
     - Decision: do not treat the draft design as a current-behavior contract. Add an explicit implementation-status note or historical banner so readers do not mistake target scope for shipped behavior.
     - Follow-up: generic `ALTER SEQUENCE` support is now tracked in `#1517`.

16. **Additional validated inner-core gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - current non-autocommit timeout handling in both text and prepared execution paths rolls the transaction back immediately on `StatementTimeoutError`;
     - local PostgreSQL 17.9 reproduction keeps the transaction open and failed after timeout, so the next statement errors with `25P02` until `ROLLBACK`;
     - local db9 reproduction on 2026-03-06 shows the opposite: after timeout, the next statement succeeds because the transaction has already been rolled back.
   - Historical evidence:
     - the behavior came in with timeout work `#861` / PR `#860`;
     - the code comment explicitly chose rollback as a safety stopgap to avoid leaving an explicit transaction in an unknown partial state;
     - that rationale was not recorded as an intentional long-term PostgreSQL divergence.
   - Decision: treat this as a real core PG-visible semantics gap, not a documentation-only mismatch.
   - Follow-up: explicit-transaction `statement_timeout` parity is now tracked in `#1519`.

17. **Additional validated savepoint/GUC semantics gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - regular `SET` (non-`LOCAL`) changed inside an explicit transaction is not restored by rollback in current db9;
     - this affects both full `ROLLBACK` and `ROLLBACK TO SAVEPOINT`;
     - local PostgreSQL 17.9 reproductions restore the pre-transaction / pre-savepoint value after rollback;
     - local db9 reproductions on 2026-03-06 keep the changed value active after the rollback.
   - Historical evidence:
     - issue `#601` and PR `#943` intentionally scoped the fix to `SET LOCAL`;
     - PR `#943` explicitly recorded this as an existing limitation, and `src/sql/session/transaction.rs` still carries `TODO(#601-followup)` for the remaining regular-`SET` savepoint undo work.
   - Decision: treat this as a real core PG-visible semantics gap, not a design-doc wording problem.
   - Follow-up: regular-`SET` transaction rollback parity is now tracked in `#1520`.

18. **Additional validated role-identity semantics gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - after `SET ROLE`, current db9 returns the effective role for `CURRENT_USER`, `SESSION_USER`, and `USER`, while PostgreSQL keeps `SESSION_USER` on the authenticated login role;
     - SQL value function `CURRENT_ROLE` is currently not implemented correctly and errors as an unresolved column reference in db9, while PostgreSQL returns the effective role;
     - PostgreSQL also supports `SET role = ...` and transaction-local `SET LOCAL role = ...` on that same effective-role surface, while current db9 returns SQL parse errors on those variants;
     - role-state changes made through `SET ROLE` / `RESET ROLE` inside an explicit transaction are not undone by `ROLLBACK` or `ROLLBACK TO SAVEPOINT` in current db9;
     - the immediate implementation cause is that `QueryContext` carries only one role identity slot and `typed_eval` maps `CURRENT_USER`, `SESSION_USER`, and `USER` to that single value.
   - Historical evidence:
     - issue `#438` / PR `#444` added minimal `SET/RESET ROLE` support to unblock privilege coverage;
     - commit `97c9783a` threaded `session.current_user()` through `QueryContext` instead of hardcoded `postgres`, but also caused `SESSION_USER` to read from the same value;
     - no explicit documentation or rationale declares this as an intentional PostgreSQL divergence.
   - Decision: treat this as a real core PG-visible semantics gap, not a documentation-only mismatch.
   - Follow-up: role identity semantics around `SET ROLE` are now tracked in `#1521`.

19. **Additional validated transaction read-only semantics gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - `SET TRANSACTION READ ONLY` / `BEGIN READ ONLY` currently mutate `default_transaction_read_only` instead of exposing transaction-local `transaction_read_only`;
     - current db9 does not implement `SHOW transaction_read_only`;
     - the changed `default_transaction_read_only` value currently leaks across `ROLLBACK`;
     - writes are still allowed inside the supposedly read-only transaction;
     - generic `SET transaction_read_only = ...` / `set_config('transaction_read_only', ..., false)` currently create sticky session-visible readback instead of PostgreSQL-scoped transaction-local behavior.
   - Historical evidence:
     - PR `#597` added transaction mode storage/readback as part of SET-family cleanup;
     - the current implementation stores access mode through `validate_transaction_modes()` into `default_transaction_read_only`;
     - no explicit documentation or rationale declares the present behavior as an intentional PostgreSQL divergence.
   - Decision: treat this as a real core PG-visible semantics gap, not a documentation-only mismatch.
   - Follow-up: transaction read-only semantics are now tracked in `#1522`.

20. **Additional validated hollow-SET gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - `SET default_transaction_isolation = ...` is currently accepted in db9;
     - `SHOW default_transaction_isolation` remains hardcoded to `read committed`;
     - future transactions remain on the engine's `repeatable read` surface regardless of the accepted setting.
   - Historical evidence:
     - PR `#597` reworked SET-family semantics and intended to remove hollow behavior;
     - PR `#617` intentionally changed `transaction_isolation` labeling to honestly expose TiKV snapshot isolation as `repeatable read`;
     - there is no evidence that the current hollow `default_transaction_isolation` accept path was an intentional design choice.
   - Decision: treat this as a real SQL-visible contract gap, not an intentional divergence.
   - Follow-up: hollow `default_transaction_isolation` SET behavior is now tracked in `#1523`.

21. **Additional validated session-default read-only gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - `SET default_transaction_read_only = on` is currently accepted in db9 and changes `SHOW default_transaction_read_only` to `on`;
     - subsequent autocommit writes still succeed;
     - explicit transactions started afterwards are still writable;
     - current db9 still does not expose `transaction_read_only` as the effective per-transaction surface.
   - Historical evidence:
     - PR `#597` framed the SET-family cleanup around every `SET` variant either changing behavior or erroring explicitly;
     - current code stores the value in `SessionSettings`, but `Session::begin()` does not consume it when opening a new transaction;
     - there is no evidence that leaving `default_transaction_read_only` as readback-only was an intentional design choice.
   - Decision: treat this as a real core SQL-visible contract gap, not a documentation-only mismatch.
   - Follow-up: session-default `default_transaction_read_only` semantics are now tracked in `#1524`.

22. **Additional validated `SET TRANSACTION` context-rule gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - outside a transaction block, PostgreSQL 17.9 emits a warning and leaves transaction defaults unchanged, while db9 currently accepts `SET TRANSACTION ...` and mutates session-visible state;
     - after the first query in an explicit transaction, PostgreSQL 17.9 rejects `SET TRANSACTION ISOLATION LEVEL ...`, while db9 currently accepts it;
     - current dispatch ignores the parser's `session` flag and runs both `SET TRANSACTION` and `SET SESSION CHARACTERISTICS AS TRANSACTION` through the same handler.
   - Historical evidence:
     - PR `#597` intentionally simplified `SET TRANSACTION` into session-mode storage/readback as part of the broader SET-family cleanup;
     - `Session::has_executed_statement_in_transaction()` exists today, but the `SET TRANSACTION` handler does not use it for PostgreSQL timing checks;
     - there is no evidence that the current outside-transaction accept path or post-first-query accept path was intended as a documented divergence.
   - Decision: treat this as a real core transaction state-machine gap, not a documentation-only mismatch.
   - Follow-up: PostgreSQL `SET TRANSACTION` context rules are now tracked in `#1525`.

23. **Additional validated `RESET` unknown-GUC gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 errors on `RESET definitely_missing_setting` with `unrecognized configuration parameter`;
     - db9 currently accepts the same statement silently and returns success;
     - current raw `RESET` handling calls `SessionSettings::reset_setting()` directly, and unknown names fall through as a silent no-op.
   - Historical evidence:
     - issue `#598` and PR `#786` added parser/executor support for `RESET <guc>` / `RESET ALL`;
     - that work fixed parse support, but there is no evidence that silent success for unknown `RESET` targets was intentional.
   - Decision: treat this as a real SQL-visible GUC-contract gap, not a documentation-only mismatch.
   - Decision detail:
     - `RESET timezone` rollback/savepoint behavior folds into the broader regular-session-state undo gap already tracked in `#1520`;
     - the new gap here is specifically the unknown-parameter error contract.
   - Follow-up: unknown-GUC `RESET` behavior is now tracked in `#1528`.

24. **Additional validated transaction-deferrable surface gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes real `transaction_deferrable` / `default_transaction_deferrable` behavior with transaction/default scoping;
     - current db9 does not implement those surfaces directly, but generic GUC handling can still accept the parameter names and expose misleading partial readback;
     - `transaction_deferrable` and `default_transaction_deferrable` are not registered in db9's known-GUC inventory.
   - Historical evidence:
     - PR `#597` intentionally retained unknown-GUC acceptance for driver compatibility via `extra_settings`;
     - there is no evidence that treating real PostgreSQL transaction parameters as generic compatibility GUCs was an intentional long-term contract choice.
   - Decision: treat this as a real SQL-visible transaction-parameter gap, not a documentation-only mismatch.
   - Follow-up: transaction-deferrable GUC behavior is now tracked in `#1529`.

25. **Additional validated reserved pseudo-GUC gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 rejects generic `SET` / `set_config()` attempts on reserved pseudo-GUC names such as `is_superuser`;
     - PostgreSQL also rejects `session_authorization` changes through generic GUC plumbing and routes that surface through real role semantics instead;
     - PostgreSQL temporarily changes `SHOW session_authorization` for `SET LOCAL session_authorization = ...` and `set_config('session_authorization', ..., true)` inside a transaction, while current db9 accepts those forms but keeps showing the authenticated login role;
     - PostgreSQL errors on `RESET is_superuser` and rejects `RESET session.authorization` as invalid syntax, while current db9 accepts both through the generic reset path;
     - current db9 accepts both names through the generic compatibility-GUC path, while `SHOW` / `current_setting()` still read back authoritative session values and silently preserve the old state.
   - Historical evidence:
     - PR `#597` intentionally retained unknown-GUC acceptance for driver compatibility via `extra_settings`;
     - there is no evidence that reserved PostgreSQL privilege/session pseudo-GUCs were intended to piggyback on that fallback.
   - Decision: treat this as a real SQL-visible session/privilege contract gap, not a documentation-only mismatch.
   - Follow-up: reserved pseudo-GUC handling is now tracked in `#1530`.

26. **Additional validated `set_config()` expression-context gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 executes `set_config()` as a real scalar function in ordinary expression contexts, including multi-projection `SELECT` lists and `SELECT ... FROM ...` queries;
     - current db9 only gives `set_config()` real side effects on the dedicated tableless fast path;
     - outside that fast path, the analyzed expression evaluator returns an empty string for `SET_CONFIG` and does not mutate session state.
   - Historical evidence:
     - PR `#943` implemented real `set_config(..., true)` local semantics through the tableless fast path;
     - PR `#947` implemented `current_setting()` in general expression contexts, but there is no evidence that leaving `set_config()` as an empty-string placeholder in those same contexts was intended as a durable PostgreSQL contract.
   - Decision: treat this as a real SQL-visible session-setting execution-path gap, not a documentation-only mismatch.
   - Follow-up: general `set_config()` execution-path parity is now tracked in `#1532`.

27. **Additional validated `SESSION AUTHORIZATION` statement-surface gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes dedicated `SET SESSION AUTHORIZATION ...`, `SET SESSION AUTHORIZATION DEFAULT`, and `RESET SESSION AUTHORIZATION` statements;
     - current db9 exposes readback-only `session_authorization` surfaces, but those dedicated statements currently fail in parsing;
     - db9 already carries a parser compatibility rewrite for `RESET ROLE`, but no analogous handling exists for `SESSION AUTHORIZATION`.
   - Historical evidence:
     - PR `#597` cleaned up generic SET-family behavior, but there is no evidence that dedicated `SESSION AUTHORIZATION` statements were intentionally excluded as a documented divergence;
     - the existing `RESET ROLE` compatibility rewrite shows the project is willing to bridge parser gaps for PostgreSQL statement surfaces when needed.
   - Decision: treat this as a real SQL-visible session-identity statement gap, not a documentation-only mismatch.
   - Follow-up: dedicated `SESSION AUTHORIZATION` statements are now tracked in `#1533`.

28. **Additional validated `session_replication_role` trigger-semantics gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes a real `session_replication_role` surface with trigger-behavior semantics; in `replica` mode, user row-level triggers are suppressed;
     - current db9 has no authoritative `session_replication_role` implementation: `SHOW session_replication_role` errors until a generic `SET` fabricates the value through compatibility-GUC storage;
     - after `SET session_replication_role = replica`, current db9 reads back `replica` but still executes BEFORE triggers normally.
   - Historical evidence:
     - PR `#597` intentionally retained unknown-GUC acceptance for driver compatibility via `extra_settings`;
     - there is no evidence that a behavior-bearing PostgreSQL trigger-control surface was intended to piggyback on that fallback.
   - Decision: treat this as a real SQL-visible trigger/session contract gap, not a documentation-only mismatch.
   - Follow-up: `session_replication_role` parity is now tracked in `#1535`.

29. **Additional validated `check_function_bodies` gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes a real `check_function_bodies` surface that changes `CREATE FUNCTION` validation behavior;
     - current db9 exposes `SET` / `RESET` / `SHOW` readback for `check_function_bodies`, but the function-creation path does not consume that setting;
     - as a result, current db9 behaves as if body validation were always disabled, even while `SHOW check_function_bodies` reports `on` by default.
   - Historical evidence:
     - `check_function_bodies` was added to the structured known-GUC registry in PR `#946` and to typed session-setting storage in PR `#943`;
     - dump/restore compatibility inputs such as `tests/96_dify_schema.sql` and `tests/dvdrental/restore.sql` set `check_function_bodies = false`, but there is no evidence that the current always-ignore behavior was intentionally documented as a durable PostgreSQL divergence.
   - Decision: treat this as a real SQL-visible DDL/session contract gap, not a documentation-only mismatch.
   - Follow-up: `check_function_bodies` parity is now tracked in `#1536`.

30. **Additional validated `standard_conforming_strings` gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes a real `standard_conforming_strings` surface that changes ordinary string-literal parsing when the setting is `off`;
     - current db9 exposes `SET` / `RESET` / `SHOW` readback for `standard_conforming_strings`, but regular string-literal semantics do not change with that setting;
     - as a result, current db9 behaves as if standard-conforming string parsing were always enabled, even while `SHOW standard_conforming_strings` can report `off`.
   - Historical evidence:
     - `standard_conforming_strings` is carried in session-setting plumbing and startup parameter reporting for PostgreSQL/driver compatibility;
     - there is no evidence that the current always-standard-conforming behavior under `SET standard_conforming_strings = off` was intentionally documented as a durable PostgreSQL divergence.
   - Decision: treat this as a real SQL-visible parser/session contract gap, not a documentation-only mismatch.
   - Follow-up: `standard_conforming_strings` parity is now tracked in `#1537`.

31. **Additional validated `bytea_output` gap discovered after the initial audit pass**
   - Verdict: Valid.
   - Root layer: implementation.
   - Facts:
     - PostgreSQL 17.9 exposes a real `bytea_output` surface that changes text-result formatting for `bytea` values between `hex` and `escape`;
     - current db9 exposes `SET` / `RESET` / `SHOW` readback for `bytea_output`, but text output for `bytea` values does not change with that setting;
     - as a result, current db9 behaves as if hex output were always enabled, even while `SHOW bytea_output` can report `escape`.
   - Historical evidence:
     - `bytea_output` is carried in session-setting plumbing and default GUC coverage for PostgreSQL compatibility;
     - there is no evidence that the current fixed-hex output under `SET bytea_output = escape` was intentionally documented as a durable PostgreSQL divergence.
   - Decision: treat this as a real SQL-visible result-format/session contract gap, not a documentation-only mismatch.
   - Follow-up: `bytea_output` parity is now tracked in `#1538`.

## Recommended Execution Order

1. **Stop contradictions in active SoT**
   - Fix `auth-rbac.md`, `sql-engine.md`, `ops-config.md`, `testing-gates.md`, `storage-format.md`, `protocol-pgwire.md`, `catalog-introspection.md`, `multi-tenancy.md`, and `modules.yaml`.

2. **Repair the canonical architecture narrative**
   - Update `docs/ARCHITECTURE.md`.
   - Update `docs/architecture/sql-engine.md`.
   - Split shipped plan-cache state from remaining roadmap work.

3. **Give shipped features an authoritative home**
   - Add HNSW coverage.
   - Add prepared plan-cache contract coverage.
   - Document the post-analysis rewriter in architecture/SoT.

4. **Fix the design-doc process**
   - Add design-doc status banners.
   - Mark obviously superseded docs.
   - Update doc-lint so active docs cannot silently rot.

## Default Decisions To Use Unless Maintainers Object

- Keep parser-boundary and prepared-statement compatibility fallbacks for now; narrow the invariant instead of deleting them.
- Treat `ops-config.md` as the authoritative registry for supported operator-facing runtime knobs, including CLI flags.
- Treat `testing-gates.md` as the repository CI/gate inventory unless required-check policy is explicitly versioned elsewhere.
- Treat HNSW as shipped and deserving full SoT/architecture coverage now.
- Treat session-local prepared plan cache as shipped, while preserving `#707` for broader future cache work.

## Execution Standards For This Workstream

These are the governing rules used for every disputed point in `#1514`:

1. Check current implementation first.
2. If the behavior is PostgreSQL-visible, verify against PostgreSQL instead of guessing.
3. Before changing code for a disputed behavior, inspect the historical issue/PR/design rationale.
4. Prefer fixing SoT and architecture docs first when implementation is already correct.
5. If PostgreSQL parity is too expensive for sound distributed design reasons, the divergence must be intentional, explicitly documented, and traceable.
6. Do not "fix" docs by copying old design intent forward when code has already evolved away from it.
7. Do not remove compatibility fallbacks lightly if they are carrying real PG surface or correctness value.

## Progress Snapshot (2026-03-06)

### Completed in this pass

The following active SoT docs have already been rewritten to current implementation truth:

- `docs/sot/auth-rbac.md`
- `docs/sot/sql-engine.md`
- `docs/sot/ops-config.md`
- `docs/sot/testing-gates.md`
- `docs/sot/storage-format.md`
- `docs/sot/protocol-pgwire.md`
- `docs/sot/catalog-introspection.md`
- `docs/sot/multi-tenancy.md`
- `docs/sot/worker-cron.md`
- `docs/sot/extensions-gin.md`

The canonical architecture and SoT index layers have also been aligned:

- `docs/ARCHITECTURE.md`
- `docs/architecture/sql-engine.md`
- `docs/sot/README.md`
- `docs/sot/modules.yaml`
- `docs/sot/invariants.md`

Design-doc hygiene has moved from one-off fixes to a directory-wide baseline:

- `docs/design/README.md` now states that `docs/design/**` is not current SoT.
- Clearly stale docs were explicitly marked `Historical` or `Superseded`:
  - `docs/design/22_distributed_async_worker.md`
  - `docs/design/24_worker_binary_spec.md`
  - `docs/design/27_hnsw_vector_index.md`
  - `docs/design/cbo-phase3-pr1-plan.md`
- Target-scope drift was called out explicitly in `docs/design/04_sequences.md`.
- The remaining draft / review / implementation-plan docs now carry an explicit non-SoT warning banner so they are not mistaken for current architecture contracts.
- `scripts/doc_lint.py` now enforces an explicit design-doc status classification on tracked `docs/design/*.md` files.
- `scripts/doc_lint.py` now validates `src/**` path references for design docs marked `Active`, while allowing `Historical` / `Superseded` records to retain archived references.
- Active-only path validation is now the documented design-doc policy; `Draft` docs remain status-only because they may legitimately reference target or future modules.
- Active design doc `docs/design/28_embedding_extension_pg_parity_contract.md` was re-checked against current code, local PostgreSQL 17.9 reproduction, and closed follow-up `#1421`; its status now reflects that the deterministic DB9 visibility contract is shipped.
- Sequence draft `docs/design/04_sequences.md` now points to follow-up `#1517` for the still-missing generic `ALTER SEQUENCE` option surface.
- This adjudication record itself was added to capture decisions, evidence, and next steps in one place.

Operational tracking is now in place:

- PR `#1515` (`fix-1514-doc-ssot-drift`) was created for this documentation-alignment batch.
- The PR carries label `experiment-fixing`.
- Sweep tracker issue `#1527` now carries the durable inner-ring transaction/session scan plan and handoff state.
- Concrete follow-up count from this adjudication pass is now `15`.
- The raw audit note `docs/design/documentation-drift-audit-2026-03-06.md` remains intentionally uncommitted.

### What those edits already fixed

- RBAC SoT no longer claims privilege enforcement is unwired; it now records the actual `require_privilege()` / `require_table_privilege()` callsites and keeps the "coverage is partial" caveat.
- SQL engine SoT now records the real analyzed query pipeline:
  - view expansion,
  - catalog snapshot build,
  - privilege checks,
  - analyzer,
  - post-analysis rewriter.
- SQL engine SoT now records:
  - `InFailedTransaction`,
  - `db9.retry_max_attempts = 64`,
  - safe retry envelope (autocommit plus first statement of an explicit transaction),
  - session-local prepared plan cache,
  - explicit parse-boundary / prepared reparse exceptions.
- Ops config SoT was rewritten into the actual operator-facing runtime registry:
  - CLI flags,
  - server/TLS/bootstrap keys,
  - worker/cron keys,
  - fs9 WebSocket keys,
  - TiKV backpressure keys,
  - tenant quota keys,
  - portal/generate_series guardrails,
  - TiKV TLS client keys.
- Testing gates SoT now points at the current workflows:
  - `ci.yml`,
  - `doc-lint.yml`,
  - `governance-lint.yml`.
- Storage SoT now records the correct serialization facts:
  - `TableSchema` = `DB9_SCHEMA_V2` + MessagePack,
  - `Row` = bincode,
  - key families are described at high level instead of pretending to mirror every encoder.
- Protocol SoT now uses current entrypoints and documents `query_parser.rs` plus the explicit utility-parse fallback boundary.
- Catalog SoT now states that the matrix is minimum/non-exhaustive and points to `CatalogRegistry` / `virtual_tables.rs` as the authoritative registry.
- Multi-tenancy SoT now includes tenant-scoped `TriggerBodyCache` isolation, not only `TableStatsCache`.
- Worker/Cron SoT now includes shipped `HnswMerge` tasks and the independent HNSW sweep loop.
- Extension/GIN SoT now uses current paths and keeps GIN planner/runtime as a shipped capability.

## Remaining Work

### Next documentation tasks

1. Continue adjudicating any still-open `#1514` controversy points one by one using the agreed order:
   1. current implementation,
   2. PostgreSQL behavior if PG-visible,
   3. historical issue / PR / design rationale.
2. If any validated mismatch turns out to require a behavior change instead of a doc fix, open a focused follow-up issue or implementation note before changing code.
3. Track removal of the prepared recursive-CTE text fallback separately; it preserves PG-visible semantics today but remains an analyzed-execution gap.
   - Tracking issue: `#1516`
4. Track generic `ALTER SEQUENCE` support separately; current code only ships `OWNED BY` / `OWNER TO`.
   - Tracking issue: `#1517`
5. Track real DML `EXPLAIN` / `EXPLAIN ANALYZE` support separately; current EXPLAIN implementation is still effectively SELECT-only.
   - Tracking issue: `#1518`
6. Track explicit-transaction `statement_timeout` parity separately; current code aborts the transaction instead of preserving PostgreSQL failed-transaction state.
   - Tracking issue: `#1519`
7. Track regular-`SET` transaction rollback parity separately; current code does not restore prior session-setting state on full `ROLLBACK` or `ROLLBACK TO SAVEPOINT`.
   - Tracking issue: `#1520`
8. Track role identity semantics around `SET ROLE` separately; current code conflates `SESSION_USER` with the effective role, does not implement `CURRENT_ROLE` correctly, and does not undo role-state changes on `ROLLBACK` or `ROLLBACK TO SAVEPOINT`.
   - Tracking issue: `#1521`
9. Track transaction read-only semantics separately; current code exposes the wrong GUC surface, leaks state across rollback, and does not enforce read-only writes.
   - Tracking issue: `#1522`
10. Track hollow `default_transaction_isolation` SET behavior separately; current code accepts the setting but leaves readback and future transaction behavior unchanged.
   - Tracking issue: `#1523`
11. Track session-default `default_transaction_read_only` semantics separately; current code changes readback but does not make future explicit transactions or autocommit statements read-only.
   - Tracking issue: `#1524`
12. Track PostgreSQL `SET TRANSACTION` context rules separately; current code accepts the statement in contexts where PostgreSQL warns or errors, and it does not separate `SET TRANSACTION` from `SET SESSION CHARACTERISTICS AS TRANSACTION`.
   - Tracking issue: `#1525`
13. Track unknown-GUC `RESET` behavior separately; current code silently succeeds where PostgreSQL errors.
   - Tracking issue: `#1528`
14. Track transaction-deferrable GUC behavior separately; current code can fake these PostgreSQL transaction parameters through generic compatibility-GUC storage.
   - Tracking issue: `#1529`
15. Track reserved pseudo-GUC handling separately; current code accepts generic `SET` / `set_config()` on names such as `is_superuser` and `session_authorization` where PostgreSQL rejects or routes through dedicated role semantics.
   - Tracking issue: `#1530`
16. Track general `set_config()` execution-path parity separately; current code only applies real side effects on the dedicated tableless fast path.
   - Tracking issue: `#1532`
17. Track dedicated `SESSION AUTHORIZATION` statements separately; current code exposes the readback surface but still fails to parse PostgreSQL's statement forms.
   - Tracking issue: `#1533`
18. Track `session_replication_role` separately; current code can fake the readback surface through generic `SET`, but it does not implement PostgreSQL's trigger-behavior semantics.
   - Tracking issue: `#1535`
19. Track `check_function_bodies` separately; current code exposes PostgreSQL's body-validation knob in session state, but `CREATE FUNCTION` does not consume it.
   - Tracking issue: `#1536`
20. Track `standard_conforming_strings` separately; current code exposes PostgreSQL's parser-mode knob in session state, but string-literal semantics do not change with it.
   - Tracking issue: `#1537`
21. Track `bytea_output` separately; current code exposes PostgreSQL's result-format knob in session state, but `bytea` text output does not change with it.
   - Tracking issue: `#1538`

### Boundaries for the next pass

- Do not treat `docs/design/**` as authoritative behavior contracts even after the banner sweep; current truth remains `docs/sot/**` plus implementation.
- Keep the raw audit note out of the PR unless there is an explicit decision to publish that working paper.
- Prefer documentation / contract fixes until a concrete PG mismatch or architectural defect has been proven.

## Verification Status

### Already done

- Current implementation evidence was re-checked from source before each SoT rewrite.
- Historical references were collected for the plan-cache and HNSW disputes:
  - `#707`
  - PR `#1078`
  - `#1220`
  - PR `#1241`
  - PR `#1298`
- Additional fallback-history references were confirmed:
  - `#759`
  - PR `#888`
  - PR `#1362`
- `uv run scripts/doc_lint.py` now passes on the current tree.
- `uv run scripts/doc_lint.py` now checks tracked design docs for explicit status classification and validates `src/**` references only for `Active` design docs.
- PostgreSQL 17.9 behavior was verified locally for prepared recursive CTE execution and prepared statements across schema drift.
- PostgreSQL 17.9 behavior was verified locally for common generic `ALTER SEQUENCE` options (`INCREMENT BY`, `START WITH`, `RESTART WITH`).
- PostgreSQL 17.9 behavior was verified locally for DML `EXPLAIN` / `EXPLAIN ANALYZE` output shape (`INSERT` / `UPDATE` / `DELETE`).
- PostgreSQL 17.9 and local db9 behavior were verified for `statement_timeout` inside explicit transactions:
  - PostgreSQL keeps the transaction open and failed until `ROLLBACK`;
  - db9 currently rolls the transaction back immediately, so the next statement succeeds.
- PostgreSQL 17.9 and local db9 behavior were verified for regular `SET` inside explicit transactions:
  - PostgreSQL restores the prior setting on both full `ROLLBACK` and `ROLLBACK TO SAVEPOINT`;
  - db9 currently leaves the changed setting active after the rollback.
- PostgreSQL 17.9 and local db9 behavior were verified for role identity semantics:
  - PostgreSQL keeps `SESSION_USER` on the authenticated login role after `SET ROLE`, while `CURRENT_USER` / `CURRENT_ROLE` / `USER` follow the effective role;
  - db9 currently returns the effective role for `CURRENT_USER` / `SESSION_USER` / `USER`, and `CURRENT_ROLE` is not implemented correctly;
  - PostgreSQL exposes a coherent SQL-visible `role` surface (`SHOW role`, `current_setting('role', true)`) that tracks the effective role, while db9 currently has no authoritative `role` surface there at all;
  - PostgreSQL also supports `SET role = ...` and `SET LOCAL role = ...`, while db9 currently returns SQL parse errors on those variants;
  - PostgreSQL undoes role-state changes on both `ROLLBACK` and `ROLLBACK TO SAVEPOINT`, while db9 currently leaves those changes active.
- PostgreSQL 17.9 and local db9 behavior were verified for transaction read-only semantics:
  - PostgreSQL exposes transaction-local `transaction_read_only`, keeps `default_transaction_read_only` unchanged, and rejects writes inside read-only transactions;
  - db9 currently mutates `default_transaction_read_only`, does not expose a real `transaction_read_only` surface, leaks the changed value across rollback, and still allows writes;
  - generic `SET transaction_read_only = ...` / `set_config('transaction_read_only', ..., false)` in db9 currently fabricate sticky session-visible readback instead of PostgreSQL's transaction-local semantics;
  - `SET LOCAL transaction_read_only = ...` and tableless `set_config('transaction_read_only', ..., true)` in db9 can also fabricate `SHOW transaction_read_only = on`, but writes still succeed inside the supposedly read-only transaction.
- PostgreSQL 17.9 and local db9 behavior were verified for `default_transaction_read_only` session defaults:
  - PostgreSQL applies the default to autocommit statements and future explicit transactions, and rejects writes under that default;
  - db9 currently changes `SHOW default_transaction_read_only` readback but still allows writes in both autocommit and later explicit transactions.
- PostgreSQL 17.9 and local db9 behavior were verified for `SET TRANSACTION` context rules:
  - PostgreSQL warns and no-ops outside transaction blocks, and rejects `SET TRANSACTION ISOLATION LEVEL ...` after the first query in an explicit transaction;
  - db9 currently accepts those statements and routes them through session-setting storage instead.
- PostgreSQL 17.9 and local db9 behavior were verified for `RESET` semantics:
  - PostgreSQL errors on unknown GUC names in `RESET`;
  - db9 currently accepts unknown `RESET` targets silently;
  - `RESET` rollback/savepoint behavior for known settings folds into the already tracked regular-session-state undo gap in `#1520`.
- PostgreSQL 17.9 and local db9 behavior were verified for transaction-deferrable surfaces:
  - PostgreSQL exposes real `transaction_deferrable` / `default_transaction_deferrable` transaction/default behavior;
  - PostgreSQL also exposes transaction-mode statement forms such as `BEGIN DEFERRABLE`, `START TRANSACTION DEFERRABLE`, `SET TRANSACTION DEFERRABLE`, and `SET SESSION CHARACTERISTICS AS TRANSACTION DEFERRABLE`;
  - db9 currently accepts those parameter names through generic GUC compatibility paths and exposes misleading partial readback instead of a real contract;
  - db9 currently returns SQL parse errors on the PostgreSQL transaction-mode statement forms for deferrable semantics.
- PostgreSQL 17.9 and local db9 behavior were verified for reserved pseudo-GUC surfaces:
  - PostgreSQL rejects generic `SET` / `set_config()` attempts on `is_superuser`;
  - PostgreSQL rejects generic `SET` / `set_config()` attempts on `session_authorization` unless real role semantics apply;
  - PostgreSQL temporarily changes `SHOW session_authorization` for `SET LOCAL session_authorization = ...` and `set_config('session_authorization', ..., true)` inside a transaction, while db9 accepts those forms but preserves the login-role readback;
  - PostgreSQL errors on `RESET is_superuser` and rejects `RESET session.authorization` as invalid syntax, while db9 currently accepts both through the generic reset path;
  - PostgreSQL treats `session.authorization` as an ordinary dotted custom-GUC name, while db9 currently hijacks it as if it were an alias for `session_authorization`;
  - PostgreSQL's special `role` surface reflects the effective role coherently, while db9's generic compatibility-GUC path can fabricate sticky or transaction-local-looking fake `role` readback through `set_config('role', ...)` without changing `current_user` / `session_user`;
  - db9 currently accepts those statements through the generic compatibility-GUC path while `SHOW` / `current_setting()` silently keep returning the authoritative old value.
- PostgreSQL 17.9 and local db9 behavior were verified for `set_config()` execution paths:
  - PostgreSQL executes `set_config()` as a real scalar function in multi-projection and `FROM`-bearing queries;
  - db9 currently only mutates session state on the dedicated tableless fast path, while general expression evaluation returns an empty string and leaves session state unchanged.
- PostgreSQL 17.9 and local db9 behavior were verified for dedicated `SESSION AUTHORIZATION` statements:
  - PostgreSQL supports `SET SESSION AUTHORIZATION <role>`, `SET SESSION AUTHORIZATION DEFAULT`, and `RESET SESSION AUTHORIZATION`;
  - db9 currently returns SQL parse errors for those statement forms even though it already exposes `SHOW session_authorization` / `current_setting('session_authorization')`.
- PostgreSQL 17.9 and local db9 behavior were verified for `session_replication_role`:
  - PostgreSQL exposes `SHOW session_replication_role` and suppresses row-level triggers in `replica` mode;
  - current db9 errors on `SHOW session_replication_role` until a generic `SET` stores the name in compatibility-GUC state;
  - after that `SET`, db9 reads back `replica` but still executes BEFORE triggers normally.
- PostgreSQL 17.9 and local db9 behavior were verified for `check_function_bodies`:
  - PostgreSQL changes `CREATE FUNCTION` validation behavior between `check_function_bodies = on` and `off`;
  - current db9 accepts and reads back the setting, but `CREATE FUNCTION` behavior does not change with it.
- PostgreSQL 17.9 and local db9 behavior were verified for `standard_conforming_strings`:
  - PostgreSQL changes ordinary string-literal parsing when the setting is `off`;
  - current db9 accepts and reads back the setting, but regular string-literal semantics do not change with it.
- PostgreSQL 17.9 and local db9 behavior were verified for `bytea_output`:
  - PostgreSQL changes `bytea` text-result formatting between `hex` and `escape`;
  - current db9 accepts and reads back the setting, but `bytea` text output does not change with it.
- PostgreSQL 17.9 and local db9 behavior were verified for `default_transaction_isolation`:
  - PostgreSQL updates the shown default and uses it for future transactions;
  - db9 currently accepts the SET statement but leaves both readback and future transaction behavior unchanged.
- PostgreSQL 17.9 and local db9 behavior were re-checked for `SET SESSION CHARACTERISTICS AS TRANSACTION ...`:
  - PostgreSQL changes both the current transaction and the session default before the first query in a transaction, but after the first query only the session default changes;
  - db9 still routes that syntax through the plain `SET TRANSACTION` handler because the parser's `session` flag is discarded;
  - this remains folded into the existing scope/context/default-surface issues already tracked in `#1525`, `#1523`, and `#1524`, so no separate issue was opened.
- Existing isolation-label divergence from PostgreSQL `read committed` was re-checked against historical issue `#608` / PR `#617` and is now documented explicitly in SoT as an intentional `transaction_isolation` contract choice.
- Follow-up issue `#1516` was opened for removing prepared recursive-CTE text fallback from analyzed execution.
- Follow-up issue `#1517` was opened for generic `ALTER SEQUENCE` support beyond `OWNED BY` / `OWNER TO`.
- Follow-up issue `#1518` was opened for real DML `EXPLAIN` / `EXPLAIN ANALYZE` support.
- Follow-up issue `#1519` was opened for explicit-transaction `statement_timeout` parity.
- Follow-up issue `#1520` was opened for regular-`SET` transaction rollback parity.
- Follow-up issue `#1521` was opened for role identity semantics around `SET ROLE`.
- Follow-up issue `#1522` was opened for transaction read-only semantics.
- Follow-up issue `#1523` was opened for hollow `default_transaction_isolation` SET behavior.
- Follow-up issue `#1524` was opened for session-default `default_transaction_read_only` semantics.
- Follow-up issue `#1525` was opened for PostgreSQL `SET TRANSACTION` context rules.
- Sweep tracker issue `#1527` was opened to carry the remaining inner-ring transaction/session scan state and avoid duplicate work.
- Follow-up issue `#1528` was opened for unknown-GUC `RESET` behavior.
- Follow-up issue `#1529` was opened for transaction-deferrable GUC behavior.
- Follow-up issue `#1530` was opened for reserved pseudo-GUC handling.
- Follow-up issue `#1532` was opened for general `set_config()` execution-path parity.
- Follow-up issue `#1533` was opened for dedicated `SESSION AUTHORIZATION` statements.
- Follow-up issue `#1535` was opened for `session_replication_role` parity.
- Follow-up issue `#1536` was opened for `check_function_bodies` parity.
- Follow-up issue `#1537` was opened for `standard_conforming_strings` parity.
- Follow-up issue `#1538` was opened for `bytea_output` parity.
- No engine behavior changes have been made in this workstream so far; this pass is documentation-only.

### Not yet done in this workstream

- The prepared recursive-CTE analyzed execution gap is not yet resolved in code; it is now tracked in `#1516`, but only the contract boundary has been clarified so far.
- Generic `ALTER SEQUENCE` support is not yet resolved in code; it is now tracked in `#1517`.
- Real DML `EXPLAIN` / `EXPLAIN ANALYZE` support is not yet resolved in code; it is now tracked in `#1518`.
- Explicit-transaction `statement_timeout` parity is not yet resolved in code; it is now tracked in `#1519`.
- Regular-`SET` transaction rollback parity is not yet resolved in code; it is now tracked in `#1520`.
- Role identity semantics around `SET ROLE` are not yet resolved in code; they are now tracked in `#1521`.
- Transaction read-only semantics are not yet resolved in code; they are now tracked in `#1522`.
- Hollow `default_transaction_isolation` SET behavior is not yet resolved in code; it is now tracked in `#1523`.
- Session-default `default_transaction_read_only` semantics are not yet resolved in code; they are now tracked in `#1524`.
- PostgreSQL `SET TRANSACTION` context rules are not yet resolved in code; they are now tracked in `#1525`.
- Unknown-GUC `RESET` behavior is not yet resolved in code; it is now tracked in `#1528`.
- Transaction-deferrable GUC behavior is not yet resolved in code; it is now tracked in `#1529`.
- Reserved pseudo-GUC handling is not yet resolved in code; it is now tracked in `#1530`.
- General `set_config()` execution-path parity is not yet resolved in code; it is now tracked in `#1532`.
- Dedicated `SESSION AUTHORIZATION` statements are not yet resolved in code; they are now tracked in `#1533`.
- `session_replication_role` parity is not yet resolved in code; it is now tracked in `#1535`.
- `check_function_bodies` parity is not yet resolved in code; it is now tracked in `#1536`.
- `standard_conforming_strings` parity is not yet resolved in code; it is now tracked in `#1537`.
- `bytea_output` parity is not yet resolved in code; it is now tracked in `#1538`.

## Practical Handoff Notes

- The active source of truth is `docs/sot/**`; use `docs/design/**` only as historical context unless a document is explicitly promoted and maintained as active.
- The current working branch is `fix-1514-doc-ssot-drift`, and the open PR is `#1515`.
- Issue `#1527` is the durable sweep tracker for the remaining inner-ring transaction/session scan.
- The raw audit note `docs/design/documentation-drift-audit-2026-03-06.md` is intentionally left out of the PR.
- Do not open with code changes unless a remaining disputed point has already been proven against both current code and PostgreSQL.
- If any remaining disputed item suggests a code change, stop and re-check:
  1. PostgreSQL behavior,
  2. current code,
  3. original issue/PR rationale.
- The user explicitly wants cautious treatment of historic design choices. "Old but intentional" is not by itself a bug.

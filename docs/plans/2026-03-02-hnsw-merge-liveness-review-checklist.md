# HNSW Merge Liveness + Compatibility Purge Review Checklist

Date: 2026-03-02  
Scope: PR #1298 follow-up (`fix(hnsw): merge liveness redesign + compatibility purge`)  
Goal: Provide a single review checklist and decision log for implementation + regression prevention.

## 1. Decision Log (Must Keep)

### D1. Schema format is V2-only (MessagePack), V1 is fully replaced
- Decision: `serialize_schema` writes only `DB9_SCHEMA_V2`.
- Decision: `deserialize_schema` rejects V1 with explicit error.
- Rationale: New product, no historical compatibility requirement; reduce regression surface.
- Review gate:
  - No V1 deserializer path in runtime code.
  - Error message for V1 data is clear and actionable.

### D2. HNSW storage is v1-only (delta-log), v0 is fully replaced
- Decision: New HNSW metadata defaults to `storage_version=1`.
- Decision: Any `storage_version != 1` fails fast in read/write/merge paths.
- Rationale: Remove dual-path complexity and silent migration behavior.
- Review gate:
  - No `storage_version == 0` migration branch in DML/scan/merge.
  - Unit test locks default version to `1`.

### D3. Merge liveness is mandatory correctness, not best effort
- Decision: HNSW merge queue deletion is CAS for `TaskType::HnswMerge` to prevent ABA.
- Decision: Startup reconciliation + periodic sweeper must detect orphaned deltas.
- Rationale: db9-server is stateless; recovery must be driven by TiKV state.

### D4. Product constraint (explicit)
- Decision: If worker is disabled during `CREATE INDEX USING hnsw`, index works with inline delta apply but background merge is unavailable.
- Decision: Enabling worker later does not retroactively register existing HNSW indexes; user must rebuild index.
- Rationale: No global DB enumeration mechanism in current worker subsystem.
- Review gate:
  - Constraint documented in code comments and release notes.
  - Warning log emitted when worker is absent.

### D5. Current technical conclusion (recorded for review baseline)
- Conclusion first: for core targets (write concurrency, TiKV hot-spot mitigation, MVCC amplification reduction), the new design is structurally better than the old design.
- Clarification: this does not mean “better in every dimension”; liveness and read-path pressure still need explicit hardening/tests.
- Why the new design is clearly better:
  - Old design rewrites the full HNSW graph blob on every DML, creating a single hot key and large MVCC version churn.
  - New design appends small delta entries; contention changes from one shared key to many unique keys, which matches distributed KV behavior.
  - db9-server stateless architecture is a better fit: graph and deltas live in TiKV, process restarts do not lose index state.
- Remaining key risks to track in review:
  - Merge scheduling lost-wakeup risk (liveness risk): deterministic queue key overwrite/delete interleaving can drop a fresh wakeup.
  - Read amplification risk: if merge lags, query path must apply many deltas (`base graph + deltas`), increasing read latency.
  - Durability window risk: if process crashes after DML commit but before merge-flush enqueue, merge trigger may be missing for a while.
  - Multi-writer order sensitivity: delta apply order is not strict commit order; HNSW insertion order may affect recall/stability consistency.
- Priority recommendation:
  - Keep periodic reconcile/sweeper as mandatory safety net to recover all missed wakeups independently of DML-triggered enqueue.

### D6. Review standard hard gates
- Decision: "No legacy retention" and "liveness under load" are release-blocking review gates.
- Decision: Unit tests alone are not sufficient for liveness acceptance.
- Review gate:
  - Legacy format guard must pass (`.ci/check_no_legacy_format.sh`).
  - HNSW liveness must include high-volume write + concurrent update integration coverage.
  - Fault-injection gaps (if any) must be explicitly tracked before final approval.

## 2. Mandatory Code Review Checklist

### A. Queue correctness (ABA / lost wakeup)
- [x] `TaskQueueEntry` has `nonce` with `#[serde(default)]`.
- [x] Enqueue path sets non-zero nonce for HNSW merge tasks.
- [x] Worker delete path uses read-compare-delete for `TaskType::HnswMerge`.
- [x] Non-HNSW tasks keep original delete behavior.

### B. Crash-orphan recovery
- [x] `CREATE INDEX USING hnsw` performs worker registry registration before tenant write commit.
- [x] Registration failure is fatal when worker exists.
- [x] Startup reconciliation scans all registry entries (no `has_hnsw_merge()` filter).
- [x] Periodic GC sweep uses same discovery path.

### C. Observability semantics
- [x] Gauge and counters are not mixed semantically.
- [x] `hnsw_pending_indexes_observed` is overwritten per sweep (`store`), not accumulated.
- [x] `hnsw_sweep_enqueued` and `hnsw_sweep_enqueue_errors` are cumulative counters.
- [x] `hnsw_scan_deltas_applied` has a single write point in HNSW scan operator.

### D. Compatibility purge consistency
- [x] No runtime path deserializes V1 schema payload.
- [x] No runtime path accepts HNSW `storage_version=0`.
- [x] `create_empty_hnsw_index()` default is v1.
- [x] CI guard script blocks legacy symbols/branches.

### E. Safety / failure handling
- [x] `hnsw_merge_task_id` handles overflow via `Result`, not panic or truncation.
- [x] Enqueue failures are logged with table/index identity.
- [x] System-store and tenant-store cross-transaction ordering is documented.

## 3. Mandatory New Tests (Beyond Existing)

### 3.1 Unit tests
- [x] `TaskQueueEntry` backward compatibility: old serialized entry without `nonce` deserializes with `nonce=0`.
- [x] `hnsw_merge_task_id` overflow returns `Err` (both table_id and index_id cases).
- [x] `create_empty_hnsw_index` default meta version is `1`.
- [x] V1 schema magic prefix is rejected with expected message.

### 3.2 Integration tests (SQL / worker behavior)
- [x] HNSW SQL suite passes: `tests/260_hnsw_basic.sql`, `tests/262_hnsw_dml.sql`, `tests/266_hnsw_batch_maintenance.sql`, `tests/268_hnsw_multi_index_isolation.sql`.
- [x] Large-write liveness script passes: `tests/hnsw_large_liveness_coverage.py` (5k rows + burst/repeated updates + nearest-neighbor checks).
- [x] Concurrent stress harness passes: `scripts/hnsw_safe_stress.sh` (mixed concurrent updates, timeout-guarded).
- [ ] `CREATE INDEX USING hnsw` with simulated registry write failure: statement fails and tenant index data is not committed.
- [ ] Crash-orphan recovery: inject failure between DML commit and merge flush, then restart worker; verify startup reconcile enqueues and merge drains deltas.
- [ ] ABA regression: deterministic queue key overwritten during merge execution does not lose newer task.
- [ ] v1-only guard: any injected `storage_version=0` metadata path fails fast in DML/scan/merge.
  Status: pending dedicated fault-injection integration suite (tracked in issue #1303).

### 3.3 Metrics tests
- [x] Sweeper updates gauge with observed backlog count (overwrite semantics).
- [x] Sweeper increments enqueue success/error counters correctly.
- [ ] Query path increments `hnsw_scan_deltas_applied` when `delta_count > 0`.

## 4. Evidence Template (Fill Before Approval)

- Commit SHA reviewed: `32341444`, `e27b6bd9`, `7fd373b0`, `43317fd4`
- Build: `cargo build` result: `PASS (2026-03-02)`
- Lint: `cargo clippy -- -D warnings` result: `PASS (2026-03-02)`
- Tests:
  - Unit tests: `cargo test` => `PASS: 2993 passed, 0 failed, 11 ignored`.
  - SQL integration full corpus: `python3 scripts/integration_test.py --dsn ... tests/` => `PASS: 306 passed, 0 failed`.
  - HNSW targeted SQL suite: `260/262/266/268` => all passed.
  - Regression gate: `bash scripts/regression_gate.sh --dsn ... --skip-orm` => passed (includes `tests/hnsw_large_liveness_coverage.py`).
  - Liveness stress: `python3 tests/hnsw_large_liveness_coverage.py --dsn ...` => passed.
  - Concurrent stress: `scripts/hnsw_safe_stress.sh` => passed.
  - ORM spot-check: `cd orm-tests && npm test -- pg-client/` => `PASS: 104/104`.
  - Legacy guard: `bash .ci/check_no_legacy_format.sh` => `PASS`.
- Manual checks:
  - Startup reconcile log evidence: `startup engine path executed; runtime HNSW merge logs observed`
  - Sweeper metrics evidence: `code-path + metrics unit tests verified gauge/counter semantics`
  - Worker-disabled constraint behavior: `documented in code comments; manual negative-mode run pending`

## 6. Review Verdict (Current)

- Verdict: **Conditionally Approved** for merge-liveness + compatibility-purge code path quality.
- Blocking follow-up before claiming full liveness closure:
  - Add fault-injection integration tests for ABA and crash-orphan windows (issue #1303).
  - Add integration assertion for `hnsw_scan_deltas_applied` runtime counter increment.

## 5. Approval Criteria

All items in sections 2 and 3 must be checked with evidence in section 4.  
If any mandatory item is missing, review status is **NOT APPROVED**.

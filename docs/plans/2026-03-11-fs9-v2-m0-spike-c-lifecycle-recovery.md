# fs9 v2 M0 Spike C: `_fs_L` Lifecycle / Recovery Failpoints (Dev TiKV + S3)

Date: 2026-03-11  
Scope: Validate the proposed fs9 v2 lifecycle model (hidden staging + atomic publish + idempotent GC) under failure and retry conditions.

Related:
- Design: `c4pt0r/db9-server#1752`
- M0 tracker: `c4pt0r/db9-server#1757`
- Local spike tool source (for reproducibility): `scripts/fs9_v2_spike_c.rs` (not product code)

## Goal

Before implementing `DataRef + _fs_L + Object/PackEntry` in `db9-server`, verify that:

- a crash between data-plane upload and TiKV publish is recoverable
- a crash between publish and old-data deletion is recoverable
- CAS failures do not corrupt metadata (never overwrite newer publishes)
- expired uploads are cleaned up safely
- GC is idempotent (delete is safe to retry; state deletion is CAS-guarded)
- optimistic txn commit conflicts can be handled with bounded retry

This spike is about **correctness under failure**, not throughput.

## Environment

Executed inside dev EKS to reuse the same network + TLS characteristics:

- Cluster: `dev-us-west-2-f02`
- Namespace: `cloud-admin-portal`
- Pod: `fs9-v2-spike-c` (rust toolchain, awscli installed)
- IRSA ServiceAccount: `db9-fs-access` (S3 access)
- Bucket: `dev-us-west-2-f02-db9-fs`
- TiKV/PD: `serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379`
- TiKV TLS: mounted from a temporary secret copied from `db9/serverless-cluster-cluster-client-secret`
- TiKV client: vendored `db9-server/vendor/tikv-client` (keyspace support)
- Keyspace: `DEFAULT`

## Model Tested (Minimal State Machine)

This spike used a simplified lifecycle state machine that matches the v2 design intent:

- `Uploading{ expected_gen, new_key, expires_ms, ... }`
- `Deleting{ delete_key, reason, ... }`
- Absence of lifecycle key means `Clean`

Key points:

- Publish is guarded by `expected_gen` (path/inode CAS proxy).
- Publish never deletes old data in the same transaction; it transitions lifecycle to `Deleting(old_key)` instead.
- CAS failure transitions lifecycle to `Deleting(new_key)` to GC the staged upload.
- GC is idempotent: S3 `delete` treats 404/NoSuchKey as success; TiKV lifecycle deletion is CAS guarded.

Note: The real design will have more states (`Packing`, reservations, tokens), but the correctness boundaries are the same.

## Failpoints / Scenarios

All scenarios were executed against real TiKV + S3:

1. Crash after upload, before publish  
   Expected: recovery sees `Uploading` + object exists, performs CAS publish, then GC old object.

2. Crash after publish, before delete  
   Expected: recovery sees `Deleting(old)`, deletes old object, clears lifecycle.

3. CAS failure (concurrent overwrite)  
   Expected: recovery must not overwrite newer inode metadata; instead GC the staged upload (`Deleting(new)`).

4. Expired upload cleanup  
   Expected: `Uploading` past expiry becomes `Deleting(new)` and is deleted.

5. Concurrent publish attempts (optimistic commit conflict / retry)  
   Expected: one publish wins; the other becomes a no-op after retry observes lifecycle mismatch; end state is correct and clean.

Additionally:

- Recovery sweep was run again after cleanup to validate idempotence (no remaining lifecycle keys).

## Results

Single run output summary (run_id: `11627603577793410693`):

- scenario1: OK
- scenario2: OK
- scenario3: OK
- scenario4: OK
- scenario5: OK
- post-clean idempotent sweep: OK (no remaining lifecycle keys)

Cleanup completed:

- S3 prefix `bench/fs9v2/spike-c/11627603577793410693/` removed
- 5 TiKV bench keys deleted

## Recommendation (Initial Defaults)

These are the initial defaults to carry into implementation (can be tuned later):

- **Optimistic commit retry**: 5 attempts, exponential backoff base 30ms  
  Rationale: handles transient conflicts without long stalls; bounded to avoid thundering retries.

- **HEAD/existence verification retry**: 5 attempts, exponential backoff base 50ms  
  Rationale: AWS S3 is strongly consistent, but some S3-compatible stores can lag; retries are cheap compared to a false-negative publish failure.

Behavioral rules to harden in product code:

- Treat S3 `delete` 404/NoSuchKey as success.
- GC must be CAS-guarded on `_fs_L` value to remain idempotent under concurrent sweeps.
- CAS failure must never “force publish”; instead it must schedule staged data for deletion.

## Follow-ups

- Extend the spike model to include `Packing` (pack flush) as another crash boundary once pack spool design is implemented.
- Add a “publish-after-verify” retry policy that distinguishes transient S3 errors from true missing objects.
- Add startup GC jitter defaults (random initial delay) to avoid herd behavior in multi-node deployments.


# fs9 v2 M0 Spike B: TiKV Inline Threshold + Batch Txn Sizing (Dev Cluster)

Date: 2026-03-11  
Scope: Validate initial `InlineBlob` sizing + batch write limits against real TiKV behavior in the dev cluster.

Related:
- Design: `c4pt0r/db9-server#1752`
- M0 tracker: `c4pt0r/db9-server#1757`

## Goal

Freeze two initial defaults based on measured TiKV transaction behavior:

- `FS9_INLINE_MAX`: maximum per-file inline payload (stored as a single TiKV value).
- `FS9_BATCH_WRITE_MAX_TOTAL_BYTES`: maximum total payload we allow in a single TiKV commit for batch write APIs.

This spike is explicitly about **TiKV txn sizing**. It does **not** include WebSocket JSON/base64 overhead.

## Environment

Executed inside the dev EKS cluster (to avoid guessing network/TLS behavior):

- Cluster: `dev-us-west-2-f02`
- Namespace: `db9`
- Pod: `fs9-v2-spike-b` (temporary, rust toolchain image)
- PD endpoint: `serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379`
- TiKV TLS: mounted from secret `serverless-cluster-cluster-client-secret` (same material used by `db9-server`)
- Client: `tikv-client` vendored from `db9-server/vendor/tikv-client` (includes keyspace support used by db9)
- Transaction mode: **optimistic** (matches current `EmbeddedPageFs` behavior)

## Method

We benchmark **put+commit** latency for:

1. **Single-key writes** at different value sizes.
2. **Batch writes** with different `(value_size, total_bytes, key_count)` shapes approximating future fs9 batch write patterns.

Parameters:

- warmup: `5` (not recorded)
- single iters: `80`
- batch iters: `25`

## Results (Run ID: 8426792610126368084)

### Single-key put+commit latency

| value_size | n | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) | errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 KiB | 80 | 6.377 | 24.092 | 24.120 | 24.162 | 0 |
| 4 KiB | 80 | 24.008 | 24.261 | 24.448 | 25.981 | 0 |
| 32 KiB | 80 | 21.995 | 22.149 | 22.308 | 22.369 | 0 |
| 64 KiB | 80 | 21.996 | 22.136 | 22.204 | 22.621 | 0 |
| 128 KiB | 80 | 22.004 | 22.197 | 23.017 | 24.040 | 0 |
| 256 KiB | 80 | 27.990 | 28.207 | 28.483 | 28.879 | 0 |

### Batch put+commit latency

| value_size | total_bytes | keys | n | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) | errors |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 KiB | 1 MiB | 256 | 25 | 95.992 | 96.634 | 100.034 | 100.034 | 0 |
| 4 KiB | 4 MiB | 1024 | 25 | 300.200 | 330.278 | 336.121 | 336.121 | 0 |
| 32 KiB | 4 MiB | 128 | 25 | 139.832 | 146.241 | 147.979 | 147.979 | 0 |
| 64 KiB | 4 MiB | 64 | 25 | 124.043 | 130.043 | 132.147 | 132.147 | 0 |
| 64 KiB | 8 MiB | 128 | 25 | 290.032 | 335.504 | 335.794 | 335.794 | 0 |
| 128 KiB | 8 MiB | 64 | 25 | 237.859 | 434.834 | 474.019 | 474.019 | 0 |

## Takeaways

- For single-key writes, latency is dominated by txn/commit and is fairly flat up to `128KiB`. `256KiB` shows a noticeable but still modest increase.
- For batch commits, **total bytes and key count** matter more than value size alone.
  - `4MiB` total payload is consistently stable in this environment (including 1024 keys of 4KiB values).
  - `8MiB` is still successful but shows larger tail latency (especially at `128KiB` values).

## Recommendation (Initial Defaults)

Conservative defaults that should be safe to ship first:

- `FS9_INLINE_MAX = 64KiB`  
  Rationale: matches “tiny/hot mutable” intent; measured latency is essentially identical to 32–128KiB; keeps Raft log entries small and reduces TiKV write amplification risk.

- `FS9_BATCH_WRITE_MAX_TOTAL_BYTES = 4MiB`  
  Rationale: stable latency across tested shapes; avoids the larger tail variance we saw at 8MiB.

Optional additional guardrail (not measured here, but implied by the 4KiB/1024-keys case):

- `FS9_BATCH_WRITE_MAX_KEYS ≈ 1024` (tunable) to avoid pathological “many tiny files” payloads.

## Follow-ups

- Repeat under concurrency (e.g., 8–32 concurrent writers) to observe contention and tail behavior.
- Add “WS overhead” spike for batch APIs: binary framing vs base64 JSON, plus server decode cost.
- Confirm that inline + inode metadata writes (multi-key txn) still fits comfortably within the chosen limits.


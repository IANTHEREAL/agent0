# fs9 v2 M0 Spike A: S3 Multipart Throughput (Dev IRSA Path)

Date: 2026-03-11  
Scope: Validate the dev-cluster S3 access path and get initial defaults for multipart part size + concurrency.

Related:
- Execution tracker: `c4pt0r/db9-server#1757` (Spike A write-up posted as a comment)
- Design: `c4pt0r/db9-server#1752`
- Dev S3/IRSA access workflow: `c4pt0r/db9-deployment-skills` PR #13

## Goal

1. Verify the dev environment can access the filesystem bucket from inside the cluster via IRSA.
2. Get an initial, measured recommendation for:
   - multipart part size
   - client concurrency
   - retry/backoff defaults (follow-up spike)

This spike is meant to de-risk the throughput target and avoid guessing parameters.

## Environment

- Kubernetes context: `arn:aws:eks:us-west-2:385595570414:cluster/dev-us-west-2-f02`
- Namespace: `cloud-admin-portal`
- ServiceAccount: `db9-fs-access` (IRSA)
- Bucket: `dev-us-west-2-f02-db9-fs`
- Test pod: `db9-fs-access-check` (aws-cli container)

## Access Verification (IRSA)

Validated:

- `aws sts get-caller-identity` returns assumed role:
  - `arn:aws:sts::385595570414:assumed-role/dev-us-west-2-f02-db9-fs-irsa/...`
- `aws s3api list-objects-v2 --bucket dev-us-west-2-f02-db9-fs` succeeds
- `aws s3 cp` put then `aws s3 rm` delete succeeds

## Method

Ran inside the IRSA pod using `aws s3 cp`, forcing multipart with a custom `AWS_CONFIG_FILE`:

- `multipart_threshold=8MB`
- varied `multipart_chunksize` (8/16/32/64/128 MB)
- varied `max_concurrent_requests` (16/32)

Timing:

- wall-clock via `date +%s%N` around each copy
- reported throughput in MiB/s (computed from bytes/time)

Notes:

- This is SigV4 auth from an in-cluster pod, not presigned URLs.
- This measures the cluster network path and S3 backend behavior; actual client machines may differ.

## Results

### 512 MiB object

`max_concurrent_requests=16`

| multipart_chunksize | upload (MiB/s) | download (MiB/s) |
|---:|---:|---:|
| 8 MB  | 116.88 | 208.63 |
| 16 MB | 117.20 | 215.97 |
| 32 MB | 116.55 | 206.03 |
| 64 MB | 125.17 | 225.31 |

### 2 GiB object

| multipart_chunksize | max_concurrent_requests | upload (MiB/s) | download (MiB/s) |
|---:|---:|---:|---:|
| 16 MB  | 16 | 144.58 | 274.70 |
| 32 MB  | 16 | 144.10 | 299.27 |
| 64 MB  | 16 | 141.25 | 313.07 |
| 128 MB | 16 | 147.12 | 294.35 |
| 64 MB  | 32 | 137.72 | 279.12 |
| 32 MB  | 32 | 142.98 | 266.64 |
| 16 MB  | 32 | 146.10 | 276.67 |

## Takeaways

- On this dev cluster -> S3 path:
  - download peaked at ~313 MiB/s (~328 MB/s)
  - upload peaked at ~147 MiB/s (~154 MB/s)
- For download, `chunksize=64MB` and `concurrency=16` was best among tested variants.
- Increasing concurrency to 32 reduced download throughput in this environment.
- Upload performance was relatively flat across 16–128MB chunks.

## Recommendation (Initial Defaults)

These are initial defaults for the **client-side** multipart strategy (to be revisited after more data):

- Part size: start at `64MB`
- Concurrency: start at `16`

Rationale:

- strong download throughput in this environment
- near-peak upload throughput
- avoids overly aggressive concurrency that can reduce performance

## Follow-ups

- Repeat the same benchmark using presigned URLs (if we want to measure the presign-specific overhead).
- Run the benchmark from the actual CLI environment (developer laptop / CI runner) for a second reference point.
- Run spike(s) for retry/backoff under throttling and transient failures.

# M1 Benchmark Local Validation Evidence

**Status**: Active
**Date**: 2026-04-19

This file is the repo-tracked evidence note referenced by
[`benchmark_m1_tpc_side_benchmarks.md`](/Users/chenhuansheng/Documents/GitHub/db9-ai/db9-server/docs/design/benchmark_m1_tpc_side_benchmarks.md).
Machine-local paths under `/Users/.../hammerdb-results` are not authoritative
evidence links for the `M1` PR; the summarized evidence below is.

## db9 Local TPROC-C Run With Official HammerDB Image

- validation date: `2026-04-18`
- db9-server commit: `d698b122`
- runner: `tpcorg/hammerdb:latest`
- dataset: local `tpcc_db9` schema on `127.0.0.1:5544`
- observed result:

```text
TEST RESULT : System achieved 9 NOPM from 0 PostgreSQL TPM
```

## db9 Local TPROC-C Run With dbsid/HammerDB Fork Scripts

- validation date: `2026-04-18`
- db9-server commit: `318c713a`
- HammerDB fork commit: `49b824e`
- runner shape: official `tpcorg/hammerdb:latest` image with
  `dbsid/HammerDB` `scripts/tcl/postgres/tprocc/*` mounted into the container
- observed result:

```text
TEST RESULT : System achieved 10 NOPM from 0 PostgreSQL TPM
```

- observed post-run correctness summary:

```json
{
  "label": "db9",
  "phase": "after_run",
  "all_passed": true
}
```

## Evidence Handling Rule

- local workstation paths may still be used as ad hoc operator scratch space
- milestone-close docs and PRs must reference repo-tracked evidence notes or
  checked-in artifacts instead of laptop-only absolute paths

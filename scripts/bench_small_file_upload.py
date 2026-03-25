#!/usr/bin/env python3
"""
Phase 0 baseline benchmark: small-file upload throughput.

Measures current batch_write_sequential performance across multiple workloads
and captures protocol overhead metrics for informing Phase 1 design.

Refs: https://github.com/c4pt0r/db9-server/issues/2107

Usage:
    # Default (10K x 4KB, single directory):
    python3 scripts/bench_small_file_upload.py

    # Full test matrix:
    python3 scripts/bench_small_file_upload.py --matrix

    # Custom workload:
    python3 scripts/bench_small_file_upload.py --files 5000 --file-size 4096 --dirs 1

    # Against remote server:
    python3 scripts/bench_small_file_upload.py --url ws://host:15480 --user admin --password admin

Requires: pip install websocket-client
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time

try:
    import websocket
except ImportError:
    print("ERROR: websocket-client required. Install: pip install websocket-client")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_req_counter = 0

def next_id():
    global _req_counter
    _req_counter += 1
    return f"bench-{_req_counter}"

def send_json(ws, payload):
    raw = json.dumps(payload)
    ws.send(raw)
    resp_raw = ws.recv()
    if isinstance(resp_raw, bytes):
        raise RuntimeError(f"Expected text frame, got binary ({len(resp_raw)} bytes)")
    return json.loads(resp_raw), len(raw), len(resp_raw)

def send_json_simple(ws, payload):
    resp, _, _ = send_json(ws, payload)
    return resp

def connect_ws(url):
    ws = websocket.WebSocket()
    ws.connect(url)
    return ws

def encode_base64(data):
    return base64.b64encode(data).decode()

def make_pattern_bytes(size_bytes):
    return bytes((idx % 251) for idx in range(size_bytes))

def chunked(seq, size):
    for start in range(0, len(seq), size):
        yield seq[start:start + size]


# ---------------------------------------------------------------------------
# Benchmark core
# ---------------------------------------------------------------------------

class BenchResult:
    def __init__(self, label, file_count, file_size_bytes, dir_count, batch_size):
        self.label = label
        self.file_count = file_count
        self.file_size_bytes = file_size_bytes
        self.dir_count = dir_count
        self.batch_size = batch_size
        # timing
        self.wall_secs = 0.0
        self.per_batch_secs = []
        # protocol metrics
        self.total_request_bytes = 0
        self.total_response_bytes = 0
        self.total_raw_payload_bytes = 0
        self.ws_request_count = 0
        # results
        self.files_written = 0
        self.errors = 0

    def files_per_sec(self):
        return self.files_written / self.wall_secs if self.wall_secs > 0 else 0

    def mib_per_sec(self):
        total = self.files_written * self.file_size_bytes
        return (total / (1024 * 1024)) / self.wall_secs if self.wall_secs > 0 else 0

    def protocol_overhead_ratio(self):
        if self.total_raw_payload_bytes == 0:
            return 0
        return self.total_request_bytes / self.total_raw_payload_bytes

    def avg_batch_latency_ms(self):
        if not self.per_batch_secs:
            return 0
        return (sum(self.per_batch_secs) / len(self.per_batch_secs)) * 1000

    def p50_batch_latency_ms(self):
        if not self.per_batch_secs:
            return 0
        s = sorted(self.per_batch_secs)
        return s[len(s) // 2] * 1000

    def p99_batch_latency_ms(self):
        if not self.per_batch_secs:
            return 0
        s = sorted(self.per_batch_secs)
        idx = min(int(len(s) * 0.99), len(s) - 1)
        return s[idx] * 1000

    def to_dict(self):
        return {
            "label": self.label,
            "file_count": self.file_count,
            "file_size_bytes": self.file_size_bytes,
            "dir_count": self.dir_count,
            "batch_size": self.batch_size,
            "wall_secs": round(self.wall_secs, 3),
            "files_per_sec": round(self.files_per_sec(), 1),
            "mib_per_sec": round(self.mib_per_sec(), 2),
            "ws_request_count": self.ws_request_count,
            "total_request_bytes": self.total_request_bytes,
            "total_response_bytes": self.total_response_bytes,
            "total_raw_payload_bytes": self.total_raw_payload_bytes,
            "protocol_overhead_ratio": round(self.protocol_overhead_ratio(), 2),
            "avg_batch_latency_ms": round(self.avg_batch_latency_ms(), 2),
            "p50_batch_latency_ms": round(self.p50_batch_latency_ms(), 2),
            "p99_batch_latency_ms": round(self.p99_batch_latency_ms(), 2),
            "files_written": self.files_written,
            "errors": self.errors,
            # Server-side metrics (Phase 0: derived from known code path)
            # Current batch_write_sequential = 1 TiKV txn per file always
            "txn_amplification_ratio": 1.0,
            "avg_files_per_txn": 1.0,
            # Future fields (placeholder for Phase 1+ schema compatibility)
            "batch_hit_rate": 1.0 if self.batch_size > 1 else 0.0,
            "fallback_reason_counts": {},
            "strategy_used": "legacy_sequential",
        }


def run_single_write_bench(ws, base_dir, files, result):
    """Benchmark: one file per WS request (single write op)."""
    t0 = time.perf_counter()
    for path, data in files:
        encoded = encode_base64(data)
        payload = {
            "id": next_id(),
            "op": "write",
            "path": path,
            "content": encoded,
            "encoding": "base64",
        }
        raw_json = json.dumps(payload)
        req_bytes = len(raw_json)
        result.total_raw_payload_bytes += len(data)

        bt = time.perf_counter()
        ws.send(raw_json)
        resp_raw = ws.recv()
        batch_dt = time.perf_counter() - bt

        result.per_batch_secs.append(batch_dt)
        result.total_request_bytes += req_bytes
        result.total_response_bytes += len(resp_raw)
        result.ws_request_count += 1

        resp = json.loads(resp_raw)
        if resp.get("ok"):
            result.files_written += 1
        else:
            result.errors += 1

    result.wall_secs = time.perf_counter() - t0


def run_batch_write_bench(ws, base_dir, files, batch_size, result):
    """Benchmark: batch_write with N files per request."""
    t0 = time.perf_counter()
    for group in chunked(files, batch_size):
        payload = {
            "id": next_id(),
            "op": "batch_write",
            "files": [
                {
                    "path": path,
                    "content": encode_base64(data),
                    "encoding": "base64",
                }
                for path, data in group
            ],
        }
        raw_json = json.dumps(payload)
        req_bytes = len(raw_json)
        raw_payload = sum(len(data) for _, data in group)
        result.total_raw_payload_bytes += raw_payload

        bt = time.perf_counter()
        ws.send(raw_json)
        resp_raw = ws.recv()
        batch_dt = time.perf_counter() - bt

        result.per_batch_secs.append(batch_dt)
        result.total_request_bytes += req_bytes
        result.total_response_bytes += len(resp_raw)
        result.ws_request_count += 1

        resp = json.loads(resp_raw)
        if resp.get("ok"):
            for entry in resp.get("data", {}).get("entries", []):
                if entry.get("ok"):
                    result.files_written += 1
                else:
                    result.errors += 1
        else:
            result.errors += len(group)

    result.wall_secs = time.perf_counter() - t0


def prepare_files(file_count, file_size, dir_count, base_dir):
    """Generate file list distributed across dir_count directories."""
    payload = make_pattern_bytes(file_size)
    files = []
    for i in range(file_count):
        dir_idx = i % dir_count
        if dir_count == 1:
            path = f"{base_dir}/f{i:06d}.bin"
        else:
            path = f"{base_dir}/d{dir_idx:04d}/f{i:06d}.bin"
        files.append((path, payload))
    return files


def setup_dirs(ws, base_dir, dir_count):
    """Create benchmark directories."""
    resp = send_json_simple(ws, {
        "id": next_id(), "op": "mkdir", "path": base_dir, "recursive": True,
    })
    if not resp.get("ok"):
        raise RuntimeError(f"Failed to create base dir: {resp}")
    if dir_count > 1:
        for i in range(dir_count):
            resp = send_json_simple(ws, {
                "id": next_id(), "op": "mkdir",
                "path": f"{base_dir}/d{i:04d}", "recursive": True,
            })


def cleanup(ws, base_dir):
    """Remove benchmark directory tree."""
    send_json_simple(ws, {
        "id": next_id(), "op": "rm", "path": base_dir, "recursive": True,
    })


def run_workload(ws, label, file_count, file_size, dir_count, batch_size, warm_run=False):
    """Run a single benchmark workload and return BenchResult."""
    run_tag = "warm" if warm_run else "cold"
    base_dir = f"/bench_{label}_{run_tag}_{int(time.time())}"

    print(f"\n{'─'*60}")
    print(f"  Workload: {label} ({run_tag})")
    print(f"  files={file_count}, size={file_size}B, dirs={dir_count}, batch={batch_size}")
    print(f"{'─'*60}")

    setup_dirs(ws, base_dir, dir_count)

    # --- Single write benchmark ---
    single_dir = f"{base_dir}/single"
    setup_dirs(ws, single_dir, dir_count)
    single_files = prepare_files(file_count, file_size, dir_count, single_dir)
    single_result = BenchResult(
        f"{label}_single_{run_tag}", file_count, file_size, dir_count, 1,
    )
    print(f"  Running single-write ({file_count} files)...")
    run_single_write_bench(ws, single_dir, single_files, single_result)
    print(
        f"  → single: {single_result.wall_secs:.2f}s, "
        f"{single_result.files_per_sec():.0f} files/s, "
        f"{single_result.mib_per_sec():.2f} MiB/s"
    )

    # --- Batch write benchmark ---
    batch_dir = f"{base_dir}/batch"
    setup_dirs(ws, batch_dir, dir_count)
    batch_files = prepare_files(file_count, file_size, dir_count, batch_dir)
    batch_result = BenchResult(
        f"{label}_batch{batch_size}_{run_tag}", file_count, file_size, dir_count, batch_size,
    )
    print(f"  Running batch-write (batch_size={batch_size})...")
    run_batch_write_bench(ws, batch_dir, batch_files, batch_size, batch_result)
    print(
        f"  → batch:  {batch_result.wall_secs:.2f}s, "
        f"{batch_result.files_per_sec():.0f} files/s, "
        f"{batch_result.mib_per_sec():.2f} MiB/s"
    )

    # Speedup
    if single_result.wall_secs > 0:
        speedup = single_result.wall_secs / max(batch_result.wall_secs, 0.001)
        print(f"  → batch speedup vs single: {speedup:.1f}x")

    # Protocol overhead
    print(f"  → protocol overhead (batch): {batch_result.protocol_overhead_ratio():.2f}x")
    print(
        f"  → batch latency: avg={batch_result.avg_batch_latency_ms():.1f}ms "
        f"p50={batch_result.p50_batch_latency_ms():.1f}ms "
        f"p99={batch_result.p99_batch_latency_ms():.1f}ms"
    )
    print(
        f"  → WS requests: single={single_result.ws_request_count} "
        f"batch={batch_result.ws_request_count}"
    )
    # Current implementation: always 1 TiKV txn per file regardless of batching
    print(
        f"  → txn amplification: 1.0 files/txn (current: no coalescing)"
    )

    cleanup(ws, base_dir)
    return single_result, batch_result


def get_provenance():
    """Collect git SHA and build info for report reproducibility."""
    def git_sha():
        try:
            return subprocess.check_output(
                ["git", "rev-parse", "--short", "HEAD"],
                stderr=subprocess.DEVNULL,
            ).decode().strip()
        except Exception:
            return "unknown"

    def git_dirty():
        try:
            out = subprocess.check_output(
                ["git", "status", "--porcelain"],
                stderr=subprocess.DEVNULL,
            ).decode().strip()
            return len(out) > 0
        except Exception:
            return False

    return {
        "db9_server_git_sha": git_sha(),
        "db9_server_git_dirty": git_dirty(),
        "build_mode": "release",
        "benchmark_script_version": "phase0_v1",
    }


# ---------------------------------------------------------------------------
# Test matrix
# ---------------------------------------------------------------------------

MATRIX = [
    # (label, file_count, file_size, dir_count, batch_size)
    ("10k_4kb_1dir",    10000, 4096,  1,   32),
    ("10k_4kb_100dir",  10000, 4096,  100, 32),
    ("10k_64b_1dir",    10000, 64,    1,   32),
    ("10k_64b_100dir",  10000, 64,    100, 32),
    ("1k_64kb_1dir",    1000,  65536, 1,   32),
    ("1k_64kb_100dir",  1000,  65536, 100, 32),
]


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Phase 0 benchmark: small-file upload throughput baseline"
    )
    parser.add_argument("--url", help="ws:// URL (default: ws://127.0.0.1:15480)")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=15480)
    parser.add_argument("--user", default="admin")
    parser.add_argument("--password", default="admin")
    parser.add_argument(
        "--matrix", action="store_true",
        help="Run full test matrix (6 workloads x cold+warm)",
    )
    parser.add_argument("--files", type=int, default=10000, help="File count (default: 10000)")
    parser.add_argument("--file-size", type=int, default=4096, help="File size in bytes")
    parser.add_argument("--dirs", type=int, default=1, help="Number of directories")
    parser.add_argument("--batch-size", type=int, default=32, help="Files per batch_write request")
    parser.add_argument(
        "--output", default=None,
        help="Write JSON results to file (default: stdout summary only)",
    )
    parser.add_argument("--warm", action="store_true", help="Include warm run (repeat same workload)")
    args = parser.parse_args()

    url = args.url or f"ws://{args.host}:{args.port}"
    print(f"Connecting to {url} ...")
    ws = connect_ws(url)

    # Auth
    resp = send_json_simple(ws, {
        "id": next_id(), "op": "auth",
        "username": args.user, "password": args.password,
    })
    if not resp.get("ok"):
        print(f"Auth failed: {resp}")
        sys.exit(1)
    auth_data = resp["data"]
    print(f"Authenticated: user={auth_data['user']}, keyspace={auth_data['keyspace']}")

    all_results = []

    if args.matrix:
        workloads = MATRIX
    else:
        workloads = [
            ("custom", args.files, args.file_size, args.dirs, args.batch_size),
        ]

    for label, fc, fs, dc, bs in workloads:
        # Cold run
        single_r, batch_r = run_workload(ws, label, fc, fs, dc, bs, warm_run=False)
        all_results.extend([single_r, batch_r])

        # Warm run (optional)
        if args.warm or args.matrix:
            single_r2, batch_r2 = run_workload(ws, label, fc, fs, dc, bs, warm_run=True)
            all_results.extend([single_r2, batch_r2])

    # --- Summary ---
    print(f"\n{'═'*70}")
    print("  PHASE 0 BASELINE SUMMARY")
    print(f"{'═'*70}")
    print(f"  {'Label':<35} {'files/s':>10} {'MiB/s':>8} {'Overhead':>9} {'p99ms':>8}")
    print(f"  {'─'*35} {'─'*10} {'─'*8} {'─'*9} {'─'*8}")
    for r in all_results:
        d = r.to_dict()
        print(
            f"  {d['label']:<35} {d['files_per_sec']:>10.0f} "
            f"{d['mib_per_sec']:>8.2f} {d['protocol_overhead_ratio']:>8.1f}x "
            f"{d['p99_batch_latency_ms']:>7.1f}"
        )
    print(f"{'═'*70}")

    # Current architecture note
    print("\n  NOTE: Current batch_write_sequential = 1 TiKV txn per file.")
    print("  Estimated txn amplification: 1.0 files/txn (no coalescing).")
    print("  Phase 1 target: N files/txn via server-side write coalescing.\n")

    # JSON output
    if args.output:
        report = {
            "benchmark": "fs9_small_file_upload_phase0",
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "server_url": url,
            "provenance": get_provenance(),
            "config": {
                "fs9_batch_write_max_files": int(os.environ.get("FS9_BATCH_WRITE_MAX_FILES", 32)),
                "fs9_inline_max": os.environ.get("FS9_INLINE_MAX", "64KiB"),
                "fs9_batch_write_max_encoded_bytes": os.environ.get(
                    "FS9_BATCH_WRITE_MAX_ENCODED_BYTES", "1MiB"
                ),
            },
            "results": [r.to_dict() for r in all_results],
        }
        with open(args.output, "w") as f:
            json.dump(report, f, indent=2)
        print(f"  JSON report written to: {args.output}")

    ws.close()

    # Exit code: fail if any errors
    total_errors = sum(r.errors for r in all_results)
    if total_errors > 0:
        print(f"\n  WARNING: {total_errors} file write errors encountered")
        sys.exit(1)


if __name__ == "__main__":
    main()

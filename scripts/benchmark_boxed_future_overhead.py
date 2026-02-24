#!/usr/bin/env python3
"""
Benchmark boxed-future overhead in db9-server query execution.

Modes:
1) Benchmark mode:
   python3 scripts/benchmark_boxed_future_overhead.py \
     --dsn postgres://admin:admin@127.0.0.1:5433/postgres \
     --output /tmp/bench.json

2) Compare mode:
   python3 scripts/benchmark_boxed_future_overhead.py \
     --compare /tmp/bench_pre.json /tmp/bench_post.json
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_DSN = "postgres://admin:admin@127.0.0.1:5433/postgres"
DEFAULT_WARMUP = 50
DEFAULT_MEASURED = 1000
DEFAULT_REPEATS = 3

DEFAULT_P95_THRESHOLD_PCT = 2.0
DEFAULT_P99_THRESHOLD_PCT = 5.0
DEFAULT_SIMPLE_ABS_MS = 0.5
DEFAULT_MIN_ABS_DELTA_MS = 0.1


@dataclass(frozen=True)
class Workload:
    wid: str
    name: str
    sql: str
    params: tuple[Any, ...]
    expected_boxes: str


WORKLOADS: list[Workload] = [
    Workload(
        wid="W1",
        name="simple_scalar",
        sql="SELECT %s::int + 1",
        params=(42,),
        expected_boxes="try_execute_analyzed + execute_via_optimizer",
    ),
    Workload(
        wid="W2",
        name="single_scalar_subquery",
        sql="SELECT (SELECT MAX(val) FROM _bench_t WHERE id > %s)",
        params=(200,),
        expected_boxes="+1 execute_subquery (ScalarSubquery pre-materialize)",
    ),
    Workload(
        wid="W3",
        name="multi_scalar_subquery",
        sql=(
            "SELECT "
            "(SELECT MAX(val) FROM _bench_t WHERE id > %s), "
            "(SELECT MIN(val) FROM _bench_t WHERE id < %s), "
            "(SELECT COUNT(*) FROM _bench_t)"
        ),
        params=(200, 800),
        expected_boxes="+3 execute_subquery calls in projection",
    ),
    Workload(
        wid="W4",
        name="in_subquery_plus_exists",
        sql=(
            "SELECT id, val FROM _bench_t "
            "WHERE id IN (SELECT fk FROM _bench_t2 WHERE fk > %s) "
            "AND EXISTS (SELECT 1 FROM _bench_t2 WHERE fk = %s)"
        ),
        params=(200, 333),
        expected_boxes="+2 execute_subquery calls (IN + EXISTS)",
    ),
    Workload(
        wid="W5",
        name="multi_cte",
        sql=(
            "WITH c1 AS (SELECT id, val FROM _bench_t WHERE id > %s), "
            "c2 AS (SELECT id, val FROM c1 WHERE val > %s) "
            "SELECT id, val FROM c2"
        ),
        params=(200, 100),
        expected_boxes="+2 execute_subquery calls via CTE materialization",
    ),
]


def percentile(values: list[float], q: float) -> float:
    if not values:
        raise ValueError("percentile() requires non-empty list")
    if len(values) == 1:
        return float(values[0])
    ordered = sorted(values)
    pos = (len(ordered) - 1) * q
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return float(ordered[lo])
    frac = pos - lo
    return float(ordered[lo] + (ordered[hi] - ordered[lo]) * frac)


def summarize_latencies_us(lat_us: list[float]) -> dict[str, float]:
    return {
        "count": float(len(lat_us)),
        "mean_us": float(statistics.fmean(lat_us)),
        "stddev_us": float(statistics.pstdev(lat_us) if len(lat_us) > 1 else 0.0),
        "p50_us": percentile(lat_us, 0.50),
        "p95_us": percentile(lat_us, 0.95),
        "p99_us": percentile(lat_us, 0.99),
        "min_us": float(min(lat_us)),
        "max_us": float(max(lat_us)),
    }


def choose_representative_repeat(repeat_summaries: list[dict[str, float]]) -> int:
    medians = [r["p50_us"] for r in repeat_summaries]
    target = statistics.median(medians)
    best_idx = 0
    best_dist = abs(medians[0] - target)
    for i in range(1, len(medians)):
        d = abs(medians[i] - target)
        if d < best_dist:
            best_idx = i
            best_dist = d
    return best_idx


def get_git_sha() -> str:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        return out
    except Exception:
        return "unknown"


def get_cpu_model() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        try:
            for line in cpuinfo.read_text(encoding="utf-8", errors="ignore").splitlines():
                if line.lower().startswith("model name"):
                    parts = line.split(":", 1)
                    if len(parts) == 2:
                        return parts[1].strip()
        except Exception:
            pass
    return platform.processor() or "unknown"


def metadata(profile: str, stack_mb_label: str) -> dict[str, Any]:
    return {
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "git_sha": get_git_sha(),
        "stack_mb": stack_mb_label,
        "pd_endpoints_env": os.environ.get("PD_ENDPOINTS", ""),
        "rust_profile": profile,
        "hostname": platform.node(),
        "cpu_model": get_cpu_model(),
        "cpu_count_logical": os.cpu_count(),
        "python": sys.version.replace("\n", " "),
        "platform": platform.platform(),
    }


def setup_bench_data(cur: Any) -> None:
    cur.execute("DROP TABLE IF EXISTS _bench_t2")
    cur.execute("DROP TABLE IF EXISTS _bench_t")
    cur.execute("CREATE TABLE _bench_t (id INT PRIMARY KEY, val INT NOT NULL)")
    cur.execute(
        "CREATE TABLE _bench_t2 (id INT PRIMARY KEY, fk INT NOT NULL, score INT NOT NULL)"
    )

    t_rows = [(i, (i * 17) % 997) for i in range(1, 1001)]
    t2_rows = [(i, ((i * 2) % 1000) + 1, (i * 13) % 251) for i in range(1, 501)]

    cur.executemany("INSERT INTO _bench_t (id, val) VALUES (%s, %s)", t_rows)
    cur.executemany(
        "INSERT INTO _bench_t2 (id, fk, score) VALUES (%s, %s, %s)",
        t2_rows,
    )
    cur.execute("CREATE INDEX _bench_t2_fk_idx ON _bench_t2(fk)")


def teardown_bench_data(cur: Any) -> None:
    cur.execute("DROP TABLE IF EXISTS _bench_t2")
    cur.execute("DROP TABLE IF EXISTS _bench_t")


def run_one_iteration(cur: Any, workload: Workload) -> float:
    start_ns = time.perf_counter_ns()
    cur.execute(workload.sql, workload.params)
    _ = cur.fetchall()
    end_ns = time.perf_counter_ns()
    return (end_ns - start_ns) / 1000.0


def run_workload(cur: Any, workload: Workload, warmup: int, measured: int, repeats: int) -> dict[str, Any]:
    repeat_results: list[dict[str, Any]] = []

    for r in range(1, repeats + 1):
        for _ in range(warmup):
            _ = run_one_iteration(cur, workload)

        samples_us: list[float] = []
        for _ in range(measured):
            samples_us.append(run_one_iteration(cur, workload))

        summary = summarize_latencies_us(samples_us)
        repeat_results.append(
            {
                "repeat": r,
                "summary": summary,
                "latency_us": samples_us,
            }
        )
        print(
            f"PASS [{workload.wid}:{workload.name}] repeat={r} "
            f"p50={summary['p50_us'] / 1000.0:.3f}ms "
            f"p95={summary['p95_us'] / 1000.0:.3f}ms "
            f"p99={summary['p99_us'] / 1000.0:.3f}ms"
        )

    summaries = [r["summary"] for r in repeat_results]
    rep_idx = choose_representative_repeat(summaries)
    rep = repeat_results[rep_idx]

    return {
        "id": workload.wid,
        "name": workload.name,
        "sql": workload.sql,
        "params": list(workload.params),
        "expected_boxes": workload.expected_boxes,
        "repeats": repeat_results,
        "representative_repeat": rep["repeat"],
        "representative_summary": rep["summary"],
    }


def benchmark_mode(args: argparse.Namespace) -> int:
    try:
        import psycopg
    except ImportError:
        print("ERROR: psycopg (v3) is required. Install with: pip install psycopg", file=sys.stderr)
        return 2

    results: dict[str, Any] = {
        "meta": metadata(args.profile, args.stack_mb_label),
        "config": {
            "dsn": args.dsn,
            "warmup": args.warmup,
            "measured": args.measured,
            "repeats": args.repeats,
            "workloads": [w.__dict__ for w in WORKLOADS],
        },
        "workloads": {},
    }

    conn = psycopg.connect(args.dsn, autocommit=True)
    cur = conn.cursor()

    started = time.monotonic()
    try:
        setup_bench_data(cur)
        for workload in WORKLOADS:
            results["workloads"][workload.wid] = run_workload(
                cur,
                workload,
                args.warmup,
                args.measured,
                args.repeats,
            )
    finally:
        if not args.no_teardown:
            try:
                teardown_bench_data(cur)
            except Exception as exc:
                print(f"WARN: teardown failed: {exc}", file=sys.stderr)
        cur.close()
        conn.close()

    elapsed = time.monotonic() - started
    results["meta"]["duration_seconds"] = elapsed

    if args.output:
        output_path = Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(results, indent=2, sort_keys=True), encoding="utf-8")
        print(f"Wrote benchmark JSON: {output_path}")
    else:
        print(json.dumps(results, indent=2, sort_keys=True))

    return 0


def load_json(path: str) -> dict[str, Any]:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def fmt_ms(us: float) -> str:
    return f"{us / 1000.0:.3f}"


def fmt_pct(v: float) -> str:
    sign = "+" if v >= 0 else ""
    return f"{sign}{v:.2f}%"


def calc_delta(pre_us: float, post_us: float) -> tuple[float, float]:
    delta_us = post_us - pre_us
    if pre_us == 0:
        pct = 0.0 if post_us == 0 else float("inf")
    else:
        pct = ((post_us - pre_us) / pre_us) * 100.0
    return delta_us, pct


def gate_regression(
    delta_us: float,
    delta_pct: float,
    pct_threshold: float,
    min_abs_delta_ms: float,
) -> bool:
    return delta_pct > pct_threshold and (delta_us / 1000.0) > min_abs_delta_ms


def compare_mode(args: argparse.Namespace) -> int:
    pre_path, post_path = args.compare
    pre = load_json(pre_path)
    post = load_json(post_path)

    workloads = sorted(set(pre.get("workloads", {}).keys()) & set(post.get("workloads", {}).keys()))
    if not workloads:
        print("ERROR: no overlapping workloads between benchmark files", file=sys.stderr)
        return 2

    print("A/B comparison (representative summaries)")
    print(f"PRE : {pre_path} (git_sha={pre.get('meta', {}).get('git_sha', 'unknown')})")
    print(f"POST: {post_path} (git_sha={post.get('meta', {}).get('git_sha', 'unknown')})")
    print("")

    header = (
        f"{'workload':<28} {'metric':<6} {'pre_ms':>10} {'post_ms':>10} "
        f"{'delta_ms':>10} {'delta_%':>9} {'gate':>6}"
    )
    print(header)
    print("-" * len(header))

    failures: list[str] = []

    for wid in workloads:
        pre_w = pre["workloads"][wid]
        post_w = post["workloads"][wid]
        pre_s = pre_w["representative_summary"]
        post_s = post_w["representative_summary"]

        metrics = [
            ("p50", "p50_us"),
            ("p95", "p95_us"),
            ("p99", "p99_us"),
        ]
        for i, (label, key) in enumerate(metrics):
            pre_v = float(pre_s[key])
            post_v = float(post_s[key])
            delta_us, delta_pct = calc_delta(pre_v, post_v)

            if label == "p95":
                gate_fail = gate_regression(
                    delta_us,
                    delta_pct,
                    args.threshold_p95_pct,
                    args.min_abs_delta_ms,
                )
            elif label == "p99":
                gate_fail = gate_regression(
                    delta_us,
                    delta_pct,
                    args.threshold_p99_pct,
                    args.min_abs_delta_ms,
                )
            else:
                gate_fail = False

            name_col = f"{wid}:{pre_w['name']}" if i == 0 else ""
            print(
                f"{name_col:<28} {label:<6} {fmt_ms(pre_v):>10} {fmt_ms(post_v):>10} "
                f"{fmt_ms(delta_us):>10} {fmt_pct(delta_pct):>9} "
                f"{('FAIL' if gate_fail else 'OK'):>6}"
            )
            if gate_fail:
                failures.append(
                    f"{wid}:{pre_w['name']} {label} regressed by {fmt_pct(delta_pct)} "
                    f"({fmt_ms(delta_us)}ms)"
                )

    # Special absolute-overhead rule for W1 simple scalar.
    w1 = pre["workloads"].get("W1")
    w1_post = post["workloads"].get("W1")
    if w1 and w1_post:
        pre_p50 = float(w1["representative_summary"]["p50_us"])
        post_p50 = float(w1_post["representative_summary"]["p50_us"])
        delta_us, _ = calc_delta(pre_p50, post_p50)
        delta_ms = delta_us / 1000.0
        ok = delta_ms <= args.threshold_simple_abs_ms
        verdict = "OK" if ok else "FAIL"
        print("")
        print(
            "Simple SELECT absolute overhead (W1 p50): "
            f"{delta_ms:.3f}ms (threshold {args.threshold_simple_abs_ms:.3f}ms) -> {verdict}"
        )
        if not ok:
            failures.append(
                "W1:simple_scalar absolute overhead exceeded threshold "
                f"({delta_ms:.3f}ms > {args.threshold_simple_abs_ms:.3f}ms)"
            )
    else:
        print("")
        print("WARN: W1 not found in one or both benchmark files; skipped absolute-overhead check")

    print("")
    print(
        "Thresholds: "
        f"p95<{args.threshold_p95_pct:.2f}% and p99<{args.threshold_p99_pct:.2f}% "
        f"(each also requires abs delta > {args.min_abs_delta_ms:.3f}ms to gate FAIL)"
    )

    compare_payload = {
        "pre": pre_path,
        "post": post_path,
        "thresholds": {
            "p95_pct": args.threshold_p95_pct,
            "p99_pct": args.threshold_p99_pct,
            "simple_abs_ms": args.threshold_simple_abs_ms,
            "min_abs_delta_ms": args.min_abs_delta_ms,
        },
        "failures": failures,
        "pass": len(failures) == 0,
    }

    if args.compare_output:
        out = Path(args.compare_output)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(compare_payload, indent=2, sort_keys=True), encoding="utf-8")
        print(f"Wrote compare summary JSON: {out}")

    if failures:
        print("Verdict: FAIL")
        for item in failures:
            print(f"  - {item}")
        return 1

    print("Verdict: PASS")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="A/B benchmark for boxed-future overhead",
    )
    parser.add_argument(
        "--dsn",
        default=os.environ.get("PG_DSN", DEFAULT_DSN),
        help="PostgreSQL DSN for benchmark mode",
    )
    parser.add_argument(
        "--output",
        default="",
        help="Output JSON path for benchmark mode",
    )
    parser.add_argument(
        "--profile",
        default=os.environ.get("DB9_RUST_PROFILE", "release"),
        help="Rust profile label stored in metadata",
    )
    parser.add_argument(
        "--stack-mb-label",
        default=os.environ.get("DB9_TOKIO_STACK_MB", ""),
        help="Stack size label stored in metadata (e.g. 8 or 32)",
    )
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP, help="Warmup iterations")
    parser.add_argument("--measured", type=int, default=DEFAULT_MEASURED, help="Measured iterations")
    parser.add_argument("--repeats", type=int, default=DEFAULT_REPEATS, help="Repeats per workload")
    parser.add_argument(
        "--no-teardown",
        action="store_true",
        help="Do not drop benchmark tables on exit",
    )
    parser.add_argument(
        "--compare",
        nargs=2,
        metavar=("PRE_JSON", "POST_JSON"),
        help="Run compare mode with two benchmark JSON files",
    )
    parser.add_argument(
        "--compare-output",
        default="",
        help="Optional output JSON path for compare verdict payload",
    )
    parser.add_argument(
        "--threshold-p95-pct",
        type=float,
        default=DEFAULT_P95_THRESHOLD_PCT,
        help="Regression gate for p95 percent delta",
    )
    parser.add_argument(
        "--threshold-p99-pct",
        type=float,
        default=DEFAULT_P99_THRESHOLD_PCT,
        help="Regression gate for p99 percent delta",
    )
    parser.add_argument(
        "--threshold-simple-abs-ms",
        type=float,
        default=DEFAULT_SIMPLE_ABS_MS,
        help="Absolute overhead gate for W1 simple scalar p50 delta in ms",
    )
    parser.add_argument(
        "--min-abs-delta-ms",
        type=float,
        default=DEFAULT_MIN_ABS_DELTA_MS,
        help="Absolute delta floor required before p95/p99 regression gates fail",
    )
    return parser


def validate_args(args: argparse.Namespace) -> None:
    if args.compare:
        return
    if args.warmup < 0 or args.measured <= 0 or args.repeats <= 0:
        raise ValueError("--warmup must be >= 0; --measured/--repeats must be > 0")


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    try:
        validate_args(args)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    if args.compare:
        return compare_mode(args)
    return benchmark_mode(args)


if __name__ == "__main__":
    sys.exit(main())

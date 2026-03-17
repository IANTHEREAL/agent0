#!/usr/bin/env python3
"""
Benchmark GIN search latency across TSVECTOR / JSONB / ARRAY workloads.

This script is intended as the Phase 0 baseline harness for issue #1906.

Example:
  python3 scripts/benchmark_gin.py \
    --dsn postgres://admin:admin@127.0.0.1:5433/postgres \
    --rows 50000 \
    --output /tmp/gin_bench.json
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
DEFAULT_ROWS = 100_000
DEFAULT_WARMUP = 3
DEFAULT_MEASURED = 5
DEFAULT_BATCH_SIZE = 1_000


@dataclass(frozen=True)
class Workload:
    wid: str
    name: str
    sql: str
    params: tuple[Any, ...] = ()


WORKLOADS: list[Workload] = [
    Workload(
        wid="Q1",
        name="fts_single_rare",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("rare_0001",),
    ),
    Workload(
        wid="Q2",
        name="fts_single_hot",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("hot_00",),
    ),
    Workload(
        wid="Q3",
        name="fts_and_rare_rare",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("rare_0001 & rare_0002",),
    ),
    Workload(
        wid="Q4",
        name="fts_and_hot_rare",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("hot_00 & rare_0001",),
    ),
    Workload(
        wid="Q5",
        name="fts_and_hot_hot",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("hot_00 & hot_01",),
    ),
    Workload(
        wid="Q6",
        name="fts_or_rare_rare",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("rare_0001 | rare_0002",),
    ),
    Workload(
        wid="Q7",
        name="fts_phrase_hot_hot",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE tsv @@ to_tsquery(%s)",
        params=("hot_00 <-> hot_01",),
    ),
    Workload(
        wid="Q8",
        name="json_contains_nested",
        sql=(
            "SELECT COUNT(*) FROM gin_bench_json "
            "WHERE data @> %s::jsonb"
        ),
        params=('{"type":"hot_00","nested":{"group":"warm_000"}}',),
    ),
    Workload(
        wid="Q9",
        name="fts_and_hot_rare_limit_10",
        sql=(
            "SELECT id FROM gin_bench_fts "
            "WHERE tsv @@ to_tsquery(%s) "
            "LIMIT 10"
        ),
        params=("hot_00 & rare_0001",),
    ),
    Workload(
        wid="Q10",
        name="fts_not_hot",
        sql="SELECT COUNT(*) FROM gin_bench_fts WHERE NOT (tsv @@ to_tsquery(%s))",
        params=("hot_00",),
    ),
    Workload(
        wid="W1",
        name="insert_10_tokens",
        sql=(
            "INSERT INTO gin_bench_write(payload, tsv, tags) "
            "VALUES (%s::jsonb, to_tsvector(%s), %s::text[])"
        ),
        params=(
            '{"kind":"ten","tokens":["hot_00","warm_000","rare_0001"]}',
            "hot_00 warm_000 rare_0001 hot_01 warm_001 hot_02 warm_002 hot_03 warm_003 hot_04",
            ["hot_00", "warm_000", "rare_0001", "hot_01", "warm_001", "hot_02", "warm_002", "hot_03", "warm_003", "hot_04"],
        ),
    ),
    Workload(
        wid="W2",
        name="insert_100_tokens",
        sql=(
            "INSERT INTO gin_bench_write(payload, tsv, tags) "
            "VALUES (%s::jsonb, to_tsvector(%s), %s::text[])"
        ),
        params=(
            json.dumps({"kind": "hundred", "tokens": [f"warm_{i:03d}" for i in range(100)]}),
            " ".join([f"warm_{i:03d}" for i in range(100)]),
            [f"warm_{i:03d}" for i in range(100)],
        ),
    ),
    Workload(
        wid="W3",
        name="insert_500_tokens",
        sql=(
            "INSERT INTO gin_bench_write(payload, tsv, tags) "
            "VALUES (%s::jsonb, to_tsvector(%s), %s::text[])"
        ),
        params=(
            json.dumps({"kind": "five_hundred", "tokens": [f"rare_{i:04d}" for i in range(500)]}),
            " ".join([f"rare_{i:04d}" for i in range(500)]),
            [f"rare_{i:04d}" for i in range(500)],
        ),
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


def summarize_latencies_ms(lat_ms: list[float]) -> dict[str, float]:
    return {
        "count": float(len(lat_ms)),
        "mean_ms": float(statistics.fmean(lat_ms)),
        "stddev_ms": float(statistics.pstdev(lat_ms) if len(lat_ms) > 1 else 0.0),
        "p50_ms": percentile(lat_ms, 0.50),
        "p95_ms": percentile(lat_ms, 0.95),
        "p99_ms": percentile(lat_ms, 0.99),
        "min_ms": float(min(lat_ms)),
        "max_ms": float(max(lat_ms)),
    }


def get_git_sha() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return "unknown"


def metadata() -> dict[str, Any]:
    return {
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "git_sha": get_git_sha(),
        "hostname": platform.node(),
        "platform": platform.platform(),
        "python": sys.version.replace("\n", " "),
        "rows_hint": DEFAULT_ROWS,
        "cwd": os.getcwd(),
    }


def setup_schema(cur: Any) -> None:
    cur.execute("DROP TABLE IF EXISTS gin_bench_fts")
    cur.execute("DROP TABLE IF EXISTS gin_bench_json")
    cur.execute("DROP TABLE IF EXISTS gin_bench_array")
    cur.execute("DROP TABLE IF EXISTS gin_bench_write")

    cur.execute(
        """
        CREATE TABLE gin_bench_fts (
            id BIGINT PRIMARY KEY,
            body TEXT NOT NULL,
            tsv TSVECTOR NOT NULL
        )
        """
    )
    cur.execute("CREATE INDEX gin_bench_fts_idx ON gin_bench_fts USING gin (tsv)")

    cur.execute(
        """
        CREATE TABLE gin_bench_json (
            id BIGINT PRIMARY KEY,
            data JSONB NOT NULL
        )
        """
    )
    cur.execute("CREATE INDEX gin_bench_json_idx ON gin_bench_json USING gin (data)")

    cur.execute(
        """
        CREATE TABLE gin_bench_array (
            id BIGINT PRIMARY KEY,
            tags TEXT[] NOT NULL
        )
        """
    )
    cur.execute("CREATE INDEX gin_bench_array_idx ON gin_bench_array USING gin (tags)")

    cur.execute(
        """
        CREATE TABLE gin_bench_write (
            id BIGSERIAL PRIMARY KEY,
            payload JSONB NOT NULL,
            tsv TSVECTOR NOT NULL,
            tags TEXT[] NOT NULL
        )
        """
    )
    cur.execute("CREATE INDEX gin_bench_write_payload_idx ON gin_bench_write USING gin (payload)")
    cur.execute("CREATE INDEX gin_bench_write_tsv_idx ON gin_bench_write USING gin (tsv)")
    cur.execute("CREATE INDEX gin_bench_write_tags_idx ON gin_bench_write USING gin (tags)")


def teardown_schema(cur: Any) -> None:
    cur.execute("DROP TABLE IF EXISTS gin_bench_write")
    cur.execute("DROP TABLE IF EXISTS gin_bench_array")
    cur.execute("DROP TABLE IF EXISTS gin_bench_json")
    cur.execute("DROP TABLE IF EXISTS gin_bench_fts")


def make_row(i: int) -> tuple[str, str, dict[str, Any], list[str]]:
    hot = [f"hot_{j:02d}" for j in range(10)]
    warm = [f"warm_{j:03d}" for j in range(100)]
    rare = [f"rare_{j:04d}" for j in range(5000)]

    hot_token = hot[i % len(hot)]
    hot_token_2 = hot[(i + 1) % len(hot)]
    warm_token = warm[(i // 3) % len(warm)]
    warm_token_2 = warm[(i // 7) % len(warm)]
    rare_token = rare[i % len(rare)]
    body_tokens = [
        hot_token,
        hot_token_2,
        warm_token,
        warm_token_2,
        rare_token,
        f"bucket_{i % 32}",
    ]
    body = " ".join(body_tokens)
    data = {
        "type": hot_token,
        "nested": {
            "group": warm_token,
            "tag": rare_token,
        },
        "bucket": i % 32,
    }
    tags = [hot_token, warm_token, rare_token]
    return body, body, data, tags


def load_data(cur: Any, rows: int, batch_size: int) -> None:
    fts_rows = []
    json_rows = []
    array_rows = []

    for i in range(1, rows + 1):
        body, tsv_text, data, tags = make_row(i)
        fts_rows.append((i, body, tsv_text))
        json_rows.append((i, json.dumps(data)))
        array_rows.append((i, tags))

        if i % batch_size == 0:
            cur.executemany(
                "INSERT INTO gin_bench_fts(id, body, tsv) VALUES (%s, %s, to_tsvector(%s))",
                fts_rows,
            )
            cur.executemany(
                "INSERT INTO gin_bench_json(id, data) VALUES (%s, %s::jsonb)",
                json_rows,
            )
            cur.executemany(
                "INSERT INTO gin_bench_array(id, tags) VALUES (%s, %s::text[])",
                array_rows,
            )
            fts_rows.clear()
            json_rows.clear()
            array_rows.clear()

    if fts_rows:
        cur.executemany(
            "INSERT INTO gin_bench_fts(id, body, tsv) VALUES (%s, %s, to_tsvector(%s))",
            fts_rows,
        )
        cur.executemany(
            "INSERT INTO gin_bench_json(id, data) VALUES (%s, %s::jsonb)",
            json_rows,
        )
        cur.executemany(
            "INSERT INTO gin_bench_array(id, tags) VALUES (%s, %s::text[])",
            array_rows,
        )


def run_one(cur: Any, workload: Workload) -> tuple[float, list[Any]]:
    start_ns = time.perf_counter_ns()
    cur.execute(workload.sql, workload.params)
    rows = cur.fetchall() if cur.description else []
    end_ns = time.perf_counter_ns()
    return (end_ns - start_ns) / 1_000_000.0, rows


def explain_query(cur: Any, workload: Workload) -> list[str]:
    cur.execute(f"EXPLAIN {workload.sql}", workload.params)
    return [row[0] for row in cur.fetchall()]


def run_workload(cur: Any, workload: Workload, warmup: int, measured: int) -> dict[str, Any]:
    for _ in range(warmup):
        _ = run_one(cur, workload)

    samples_ms = []
    last_rows = []
    for _ in range(measured):
        lat_ms, rows = run_one(cur, workload)
        samples_ms.append(lat_ms)
        last_rows = rows

    result: dict[str, Any] = {
        "id": workload.wid,
        "name": workload.name,
        "sql": workload.sql,
        "params": list(workload.params),
        "summary": summarize_latencies_ms(samples_ms),
        "row_count": len(last_rows),
    }
    if workload.wid.startswith("Q"):
        result["explain"] = explain_query(cur, workload)
    return result


def benchmark_mode(args: argparse.Namespace) -> int:
    try:
        import psycopg
    except ImportError:
        print("ERROR: psycopg (v3) is required. Install with: pip install psycopg", file=sys.stderr)
        return 2

    results: dict[str, Any] = {
        "meta": metadata(),
        "config": {
            "dsn": args.dsn,
            "rows": args.rows,
            "batch_size": args.batch_size,
            "warmup": args.warmup,
            "measured": args.measured,
        },
        "workloads": {},
    }

    conn = psycopg.connect(args.dsn, autocommit=True)
    cur = conn.cursor()
    started = time.monotonic()
    try:
        setup_schema(cur)
        load_data(cur, args.rows, args.batch_size)
        for workload in WORKLOADS:
            results["workloads"][workload.wid] = run_workload(
                cur,
                workload,
                args.warmup,
                args.measured,
            )
    finally:
        if not args.no_teardown:
            try:
                teardown_schema(cur)
            except Exception as exc:
                print(f"WARN: teardown failed: {exc}", file=sys.stderr)
        cur.close()
        conn.close()

    results["meta"]["duration_seconds"] = time.monotonic() - started

    payload = json.dumps(results, indent=2, sort_keys=True)
    if args.output:
        output_path = Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(payload, encoding="utf-8")
        print(f"Wrote benchmark JSON: {output_path}")
    else:
        print(payload)
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="db9-server GIN benchmark harness")
    parser.add_argument("--dsn", default=DEFAULT_DSN, help="PostgreSQL DSN")
    parser.add_argument("--rows", type=int, default=DEFAULT_ROWS, help="Number of rows to generate")
    parser.add_argument("--batch-size", type=int, default=DEFAULT_BATCH_SIZE, help="Bulk insert batch size")
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP, help="Warmup iterations per workload")
    parser.add_argument("--measured", type=int, default=DEFAULT_MEASURED, help="Measured iterations per workload")
    parser.add_argument("--output", help="Write JSON benchmark output to this path")
    parser.add_argument("--no-teardown", action="store_true", help="Preserve benchmark tables after the run")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    return benchmark_mode(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

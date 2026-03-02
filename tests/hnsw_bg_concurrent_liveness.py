#!/usr/bin/env python3
"""
Concurrent background-write liveness coverage for HNSW delta-log path.

Goals:
1. Exercise many concurrent commits that all target the same HNSW index
   (high queue-key overwrite pressure).
2. Verify all background writes complete (no lost task visibility).
3. Verify read-path correctness after concurrent writes.
"""

import argparse
import random
import string
import subprocess
import sys
import time
from typing import List, Tuple


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Concurrent background-write liveness coverage for HNSW"
    )
    parser.add_argument(
        "--dsn",
        required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def run_sql(dsn: str, sql: str) -> str:
    result = subprocess.run(
        [
            "psql",
            dsn,
            "--no-psqlrc",
            "-A",
            "-t",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"psql failed (exit {result.returncode})\nSQL: {sql}\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )
    return result.stdout.strip()


def expect_int_eq(dsn: str, sql: str, expected: int, msg: str) -> None:
    out = run_sql(dsn, sql)
    try:
        got = int(out)
    except ValueError as exc:
        raise AssertionError(f"{msg}: expected int output, got {out!r}") from exc
    assert got == expected, f"{msg}: expected {expected}, got {got}"


def expect_int_in_range(dsn: str, sql: str, lo: int, hi: int, msg: str) -> None:
    out = run_sql(dsn, sql)
    try:
        got = int(out)
    except ValueError as exc:
        raise AssertionError(f"{msg}: expected int output, got {out!r}") from exc
    assert lo <= got <= hi, f"{msg}: expected {lo}..{hi}, got {got}"


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def launch_bg_sql(dsn: str, sql_body: str) -> int:
    wrapped = f"SELECT pg_background_launch($${sql_body}$$);"
    out = run_sql(dsn, wrapped)
    try:
        return int(out)
    except ValueError as exc:
        raise AssertionError(f"pg_background_launch returned non-int: {out!r}") from exc


def poll_bg_result(dsn: str, task_id: int, timeout_sec: float = 40.0) -> str:
    deadline = time.time() + timeout_sec
    last = ""
    while time.time() < deadline:
        last = run_sql(dsn, f"SELECT pg_background_result({task_id});")
        if last == "pending":
            time.sleep(0.1)
            continue
        return last
    raise AssertionError(
        f"task {task_id} did not finish within timeout, last result={last!r}"
    )


def make_update_jobs(table_name: str) -> List[Tuple[int, int, int, str]]:
    jobs: List[Tuple[int, int, int, str]] = []
    # 12 disjoint updates; all touch the same HNSW index, creating queue overwrite pressure.
    # Segment size=50 over id 1..600.
    for i in range(12):
        marker = i + 1
        start_id = i * 50 + 1
        end_id = start_id + 49
        x = 0.10 + i * 0.07
        y = 0.20 + i * 0.05
        z = 0.30 + i * 0.03
        sql = (
            f"UPDATE {table_name} "
            f"SET v='[{x:.3f},{y:.3f},{z:.3f}]'::vector(3), marker={marker} "
            f"WHERE id BETWEEN {start_id} AND {end_id};"
        )
        jobs.append((marker, start_id, end_id, sql))
    return jobs


def main() -> int:
    args = parse_args()
    dsn = args.dsn

    suffix = random_suffix()
    table_name = f"hnsw_bg_live_{suffix}"
    index_name = f"idx_{table_name}"

    print(f"[INFO] table={table_name}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        run_sql(
            dsn,
            f"""
            CREATE TABLE {table_name} (
                id INT PRIMARY KEY,
                v VECTOR(3),
                marker INT NOT NULL DEFAULT 0
            );
            """,
        )
        run_sql(
            dsn,
            f"""
            INSERT INTO {table_name} (id, v, marker)
            SELECT
                g,
                format(
                    '[%s,%s,%s]',
                    (g % 101)::float / 101.0,
                    ((g + 1) % 101)::float / 101.0,
                    ((g + 2) % 101)::float / 101.0
                )::vector(3),
                0
            FROM generate_series(1, 2000) g;
            """,
        )
        run_sql(
            dsn,
            f"CREATE INDEX {index_name} ON {table_name} USING hnsw (v vector_l2_ops);",
        )
        expect_int_eq(dsn, f"SELECT COUNT(*) FROM {table_name};", 2000, "seed row count")

        jobs = make_update_jobs(table_name)
        task_ids: List[Tuple[int, int, int, int]] = []

        for marker, start_id, end_id, sql in jobs:
            task_id = launch_bg_sql(dsn, sql)
            task_ids.append((task_id, marker, start_id, end_id))

        # Poll every task to completion. 'not found' is treated as failure for launched tasks.
        for task_id, marker, start_id, end_id in task_ids:
            result = poll_bg_result(dsn, task_id)
            assert result != "not found", (
                f"task {task_id} became not found (marker={marker}, range={start_id}-{end_id})"
            )

        # Validate all disjoint segments were updated as expected.
        expect_int_eq(
            dsn,
            f"SELECT COUNT(*) FROM {table_name} WHERE marker BETWEEN 1 AND 12;",
            600,
            "concurrent segment updates applied",
        )

        for _, marker, start_id, end_id in task_ids:
            expect_int_eq(
                dsn,
                (
                    f"SELECT COUNT(*) FROM {table_name} "
                    f"WHERE id BETWEEN {start_id} AND {end_id} AND marker = {marker};"
                ),
                50,
                f"marker={marker} segment correctness",
            )

        # Read-path correctness under pending/merged delta states.
        for i, (_, marker, start_id, end_id) in enumerate(task_ids):
            x = 0.10 + i * 0.07
            y = 0.20 + i * 0.05
            z = 0.30 + i * 0.03
            expect_int_in_range(
                dsn,
                (
                    f"SELECT id FROM {table_name} "
                    f"ORDER BY v <-> '[{x:.3f},{y:.3f},{z:.3f}]' LIMIT 1;"
                ),
                start_id,
                end_id,
                f"nearest neighbor in marker={marker} segment",
            )

        # Additional hot-row write/read check.
        for i in range(1, 61):
            run_sql(
                dsn,
                (
                    f"UPDATE {table_name} "
                    f"SET v='[{50.0 + i:.1f},0.0,0.0]'::vector(3), marker={9000 + i} "
                    f"WHERE id = 1;"
                ),
            )

        expect_int_eq(
            dsn,
            f"SELECT marker FROM {table_name} WHERE id = 1;",
            9060,
            "hot-row latest marker visible",
        )
        expect_int_eq(
            dsn,
            f"SELECT id FROM {table_name} ORDER BY v <-> '[110.0,0.0,0.0]' LIMIT 1;",
            1,
            "hot-row nearest neighbor after repeated updates",
        )

        print("PASS: HNSW concurrent background-write liveness coverage")
        return 0
    except AssertionError as exc:
        print(f"FAIL: assertion failed: {exc}")
        return 1
    except Exception as exc:
        print(f"FAIL: runtime error: {exc}")
        return 1
    finally:
        try:
            run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        except Exception:
            pass


if __name__ == "__main__":
    raise SystemExit(main())

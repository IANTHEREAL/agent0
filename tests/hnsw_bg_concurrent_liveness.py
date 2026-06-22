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
import os
import random
import string
import subprocess
import sys
import time
from typing import List, Tuple

BG_TASK_TIMEOUT_SEC = float(os.getenv("DB9_HNSW_BG_TASK_TIMEOUT_SEC", "180"))
BG_TASK_POLL_INTERVAL_SEC = float(os.getenv("DB9_HNSW_BG_TASK_POLL_INTERVAL_SEC", "0.25"))


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


def run_sql(dsn: str, sql: str, timeout: float = 120) -> str:
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
        timeout=timeout,
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


def drop_table_best_effort(dsn: str, table_name: str) -> None:
    try:
        run_sql(
            dsn,
            f"SET statement_timeout = 0; DROP TABLE IF EXISTS {table_name};",
            timeout=180,
        )
    except Exception:
        pass


def poll_bg_results(
    dsn: str,
    task_ids: List[Tuple[int, int, int, int]],
    timeout_sec: float = BG_TASK_TIMEOUT_SEC,
) -> None:
    deadline = time.time() + timeout_sec
    pending = {
        task_id: (marker, start_id, end_id)
        for task_id, marker, start_id, end_id in task_ids
    }
    last_results = {task_id: "pending" for task_id, _, _, _ in task_ids}

    while pending and time.time() < deadline:
        for task_id in list(pending):
            result = run_sql(dsn, f"SELECT pg_background_result({task_id});")
            last_results[task_id] = result
            if result == "pending":
                continue

            marker, start_id, end_id = pending.pop(task_id)
            assert result != "not found", (
                f"task {task_id} became not found "
                f"(marker={marker}, range={start_id}-{end_id})"
            )
            assert result == "OK", (
                f"task {task_id} returned {result!r} "
                f"(marker={marker}, range={start_id}-{end_id})"
            )

        if pending:
            time.sleep(BG_TASK_POLL_INTERVAL_SEC)

    if pending:
        details = []
        for task_id, (marker, start_id, end_id) in sorted(pending.items()):
            details.append(
                f"task={task_id} marker={marker} range={start_id}-{end_id} "
                f"last={last_results[task_id]!r}"
            )
        raise AssertionError(
            f"{len(pending)} background task(s) did not finish within "
            f"{timeout_sec:.0f}s: {', '.join(details)}"
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
        drop_table_best_effort(dsn, table_name)
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
        poll_bg_results(dsn, task_ids)

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
            drop_table_best_effort(dsn, table_name)
        except Exception:
            pass


if __name__ == "__main__":
    raise SystemExit(main())

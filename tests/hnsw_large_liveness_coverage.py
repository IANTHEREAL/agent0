#!/usr/bin/env python3
"""
Large-write and liveness-style HNSW regression coverage.

Goals:
1. Exercise HNSW under larger batch sizes (thousands of rows).
2. Validate read-path correctness after burst updates (query sees latest writes).
3. Keep runtime deterministic and CI-friendly.
"""

import argparse
import os
import random
import string
import subprocess
import sys
import time


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Large-write and liveness-style HNSW regression coverage"
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


def expect_int_in_range(
    dsn: str, sql: str, lo: int, hi: int, msg: str, allow_none: bool = False
) -> None:
    out = run_sql(dsn, sql)
    if out == "" and allow_none:
        return
    try:
        got = int(out)
    except ValueError as exc:
        raise AssertionError(f"{msg}: expected int output, got {out!r}") from exc
    assert lo <= got <= hi, f"{msg}: expected {lo}..{hi}, got {got}"


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def main() -> int:
    args = parse_args()
    dsn = args.dsn

    suffix = random_suffix()
    table_name = f"hnsw_large_live_{suffix}"
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
            f"CREATE INDEX {index_name} ON {table_name} USING hnsw (v vector_l2_ops);",
        )

        # Seed 4,000 rows (large enough to stress row + index write path, still CI-friendly).
        run_sql(
            dsn,
            f"""
            INSERT INTO {table_name} (id, v, marker)
            SELECT
                g,
                format(
                    '[%s,%s,%s]',
                    (g % 97)::float / 97.0,
                    ((g + 1) % 97)::float / 97.0,
                    ((g + 2) % 97)::float / 97.0
                )::vector(3),
                0
            FROM generate_series(1, 4000) g;
            """,
        )
        expect_int_eq(
            dsn,
            f"SELECT COUNT(*) FROM {table_name};",
            4000,
            "seed row count",
        )

        # Burst updates on a hot subset to simulate delta backlog pressure.
        run_sql(
            dsn,
            f"UPDATE {table_name} SET v='[9,0,0]', marker=1 WHERE id BETWEEN 1 AND 600;",
        )
        run_sql(
            dsn,
            f"UPDATE {table_name} SET v='[0,9,0]', marker=2 WHERE id BETWEEN 1 AND 600;",
        )
        run_sql(
            dsn,
            f"UPDATE {table_name} SET v='[0,0,9]', marker=3 WHERE id BETWEEN 1 AND 600;",
        )

        expect_int_eq(
            dsn,
            f"SELECT marker FROM {table_name} WHERE id = 1;",
            3,
            "latest marker visible after burst updates",
        )
        expect_int_in_range(
            dsn,
            f"SELECT id FROM {table_name} ORDER BY v <-> '[0,0,9]' LIMIT 1;",
            1,
            600,
            "nearest after burst update should come from latest-updated subset",
        )

        # Multi-row update over another wide range.
        run_sql(
            dsn,
            f"""
            UPDATE {table_name}
            SET v='[0.123,0.456,0.789]', marker=4
            WHERE id BETWEEN 2200 AND 3200;
            """,
        )
        expect_int_in_range(
            dsn,
            f"SELECT id FROM {table_name} ORDER BY v <-> '[0.123,0.456,0.789]' LIMIT 1;",
            2200,
            3200,
            "nearest after wide-range update should come from updated range",
        )

        # Insert additional 1,000 rows in a single statement.
        run_sql(
            dsn,
            f"""
            INSERT INTO {table_name} (id, v, marker)
            SELECT g, '[0.5,0.5,0.5]'::vector(3), 5
            FROM generate_series(4001, 5000) g;
            """,
        )
        expect_int_eq(
            dsn,
            f"SELECT COUNT(*) FROM {table_name};",
            5000,
            "row count after large insert",
        )
        expect_int_in_range(
            dsn,
            f"SELECT id FROM {table_name} ORDER BY v <-> '[0.5,0.5,0.5]' LIMIT 1;",
            4001,
            5000,
            "nearest for inserted batch probe",
        )

        # Repeated singleton updates: ensure latest write is visible on read-path.
        for i in range(1, 41):
            run_sql(
                dsn,
                f"""
                UPDATE {table_name}
                SET v = format('[%s,%s,%s]', {i}::float, 0.0, 0.0)::vector(3),
                    marker = {1000 + i}
                WHERE id = 1;
                """,
            )
        expect_int_eq(
            dsn,
            f"SELECT marker FROM {table_name} WHERE id = 1;",
            1040,
            "latest marker visible after repeated singleton updates",
        )

        print("PASS: HNSW large-write and liveness-style coverage")
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


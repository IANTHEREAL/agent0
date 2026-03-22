#!/usr/bin/env python3
"""
Regression coverage for the KV value size guard (#1968 / PR #1979).

Validates that an oversized row write is rejected pre-flight with the new,
actionable db9 error instead of surfacing TiKV's cryptic RaftEntryTooLarge
commit-time failure.
"""

import argparse
import random
import string
import subprocess
import sys
import time


OVERSIZED_CHARS = 8_500_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Regression coverage for the KV value size guard"
    )
    parser.add_argument(
        "--dsn",
        required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def run_psql(dsn: str, sql: str, expect_success: bool = True) -> subprocess.CompletedProcess:
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
        timeout=180,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(
            f"psql failed (exit {result.returncode})\nSQL: {sql}\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )
    return result


def run_sql(dsn: str, sql: str) -> str:
    return run_psql(dsn, sql, expect_success=True).stdout.strip()


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    table_name = f"txn_value_guard_{random_suffix()}"

    print(f"[INFO] table={table_name}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        run_sql(
            dsn,
            f"CREATE TABLE {table_name} (id INT PRIMARY KEY, payload TEXT NOT NULL);",
        )

        run_sql(dsn, f"INSERT INTO {table_name} VALUES (1, 'ok');")
        count = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count == "1", f"expected 1 seeded row, got {count!r}"

        oversized_insert = f"""
            INSERT INTO {table_name}
            VALUES (2, repeat('x', {OVERSIZED_CHARS}));
        """
        result = run_psql(dsn, oversized_insert, expect_success=False)
        if result.returncode == 0:
            raise AssertionError("oversized insert unexpectedly succeeded")

        stderr = (result.stderr or "").lower()
        assert "value too large" in stderr, f"expected guard error, got: {result.stderr!r}"
        assert "table row" in stderr, f"expected subsystem hint, got: {result.stderr!r}"
        assert (
            "db9_txn_value_size_limit_bytes" in stderr
        ), f"expected env-var hint, got: {result.stderr!r}"
        assert "raftentrytoolarge" not in stderr, (
            "expected pre-flight db9 guard, not TiKV commit-time raft error: "
            f"{result.stderr!r}"
        )

        count_after = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count_after == "1", (
            f"failed oversized insert must not commit a row, got count={count_after!r}"
        )

        print("PASS: oversized row insert rejected by txn value size guard")
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

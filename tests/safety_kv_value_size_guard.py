#!/usr/bin/env python3
"""
Additional safety regression tests for KV value size guard (PRs #1979, #2025).

Supplements existing tests with:
1. Connection remains usable after oversized write rejection
2. Batch INSERT with one oversized row rejects entire statement
3. UPDATE that makes a row exceed the limit
4. Verify SQLSTATE 54000 (program_limit_exceeded)
"""

import argparse
import random
import string
import subprocess
import time


OVERSIZED_CHARS = 8_500_000  # ~8.5 MB, exceeds default 8 MiB limit


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="KV value size guard additional safety tests"
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
            f"psql failed (exit {result.returncode})\n"
            f"SQL: {sql}\nstderr: {result.stderr}"
        )
    return result


def run_sql(dsn: str, sql: str) -> str:
    return run_psql(dsn, sql, expect_success=True).stdout.strip()


def run_verbose_psql(dsn: str, sql: str) -> subprocess.CompletedProcess:
    """Run psql with VERBOSITY verbose to capture SQLSTATE codes."""
    full_input = f"\\set VERBOSITY verbose\n{sql}\n"
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-f", "-"],
        input=full_input,
        capture_output=True,
        text=True,
        timeout=180,
    )


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def test_connection_recovery(dsn: str, table: str) -> None:
    """After an oversized write rejection, the connection should remain usable."""
    # Use a single psql session: oversized insert (fails), then normal insert (should succeed)
    full_input = (
        f"INSERT INTO {table} VALUES (100, repeat('x', {OVERSIZED_CHARS}));\n"
        f"INSERT INTO {table} VALUES (101, 'small_after_rejection');\n"
        f"SELECT id FROM {table} WHERE id = 101;\n"
    )
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=full_input,
        capture_output=True,
        text=True,
        timeout=180,
    )
    # The first INSERT should fail, but the second should succeed
    assert "101" in result.stdout, (
        f"Connection should recover after rejection. stdout: {result.stdout!r}"
    )

    # Verify via separate connection
    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 101")
    assert count == "1", f"Small insert after rejection should commit, got {count}"

    print("  [OK] Test 1: Connection recovers after oversized write rejection")


def test_batch_insert_rejection(dsn: str, table: str) -> None:
    """Batch INSERT with one oversized row should reject the entire statement."""
    result = run_verbose_psql(
        dsn,
        f"INSERT INTO {table} VALUES (200, 'ok'), (201, repeat('x', {OVERSIZED_CHARS}));",
    )
    assert result.returncode != 0, "Batch INSERT with oversized row should fail"

    stderr = (result.stderr or "").lower()
    assert "value too large" in stderr, f"Expected guard error, got: {result.stderr!r}"
    assert "54000" in stderr, f"Expected SQLSTATE 54000, got: {result.stderr!r}"

    # Verify neither row was committed
    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id IN (200, 201)")
    assert count == "0", f"No rows should be committed from failed batch, got {count}"

    print("  [OK] Test 2: Batch INSERT with oversized row rejects entire statement")


def test_update_to_oversized(dsn: str, table: str) -> None:
    """UPDATE that makes a row exceed the limit should be rejected."""
    run_sql(dsn, f"INSERT INTO {table} VALUES (300, 'small_initially')")

    result = run_verbose_psql(
        dsn,
        f"UPDATE {table} SET data = repeat('x', {OVERSIZED_CHARS}) WHERE id = 300;",
    )
    assert result.returncode != 0, "UPDATE to oversized should fail"

    stderr = (result.stderr or "").lower()
    assert "value too large" in stderr, f"Expected guard error, got: {result.stderr!r}"
    assert "54000" in stderr, f"Expected SQLSTATE 54000, got: {result.stderr!r}"

    # Verify original data unchanged
    data = run_sql(dsn, f"SELECT data FROM {table} WHERE id = 300")
    assert data == "small_initially", f"Original data should be unchanged, got {data!r}"

    print("  [OK] Test 3: UPDATE to oversized row rejected with SQLSTATE 54000")


def test_normal_sized_succeeds(dsn: str, table: str) -> None:
    """Normal-sized rows (well under limit) should always succeed."""
    # 1 MB row - well under 8 MiB limit
    run_sql(dsn, f"INSERT INTO {table} VALUES (400, repeat('n', 1000000))")
    length = run_sql(dsn, f"SELECT length(data) FROM {table} WHERE id = 400")
    assert length == "1000000", f"Expected 1MB row to succeed, got length={length}"

    print("  [OK] Test 4: Normal-sized row (1 MB) succeeds")


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    table = f"kvguard_{random_suffix()}"

    print(f"[INFO] table={table}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"CREATE TABLE {table} (id INT PRIMARY KEY, data TEXT NOT NULL)")

        test_connection_recovery(dsn, table)
        test_batch_insert_rejection(dsn, table)
        test_update_to_oversized(dsn, table)
        test_normal_sized_succeeds(dsn, table)

        print("PASS: KV value size guard additional safety tests (4/4)")
        return 0
    except AssertionError as exc:
        print(f"FAIL: assertion failed: {exc}")
        return 1
    except Exception as exc:
        print(f"FAIL: runtime error: {exc}")
        return 1
    finally:
        try:
            run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        except Exception:
            pass


if __name__ == "__main__":
    raise SystemExit(main())

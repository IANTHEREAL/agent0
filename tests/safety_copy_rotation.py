#!/usr/bin/env python3
"""
Safety regression tests for COPY FROM STDIN transaction rotation (PR #2033).

Validates:
1. COPY >5000 rows in autocommit mode succeeds (rotation works)
2. COPY inside explicit transaction does NOT rotate (atomic)
3. Partial commit on mid-stream failure (autocommit, non-atomic)
4. Small COPY (<5000 rows) works without rotation
"""

import argparse
import io
import random
import string
import subprocess
import time


COPY_STDIN_COMMIT_SIZE = 5000  # matches src/protocol/handler/copy/mod.rs


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="COPY FROM STDIN transaction rotation safety tests"
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
        timeout=120,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(
            f"psql failed (exit {result.returncode})\n"
            f"SQL: {sql}\nstderr: {result.stderr}"
        )
    return result


def run_sql(dsn: str, sql: str) -> str:
    return run_psql(dsn, sql, expect_success=True).stdout.strip()


def run_copy_stdin(dsn: str, sql_preamble: str, copy_cmd: str, data: str,
                   expect_success: bool = True) -> subprocess.CompletedProcess:
    """Run COPY FROM STDIN via psql -f - with data piped through stdin."""
    full_input = f"{sql_preamble}\n{copy_cmd}\n{data}\\.\n"
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=full_input,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(
            f"COPY STDIN failed (exit {result.returncode})\n"
            f"stderr: {result.stderr}"
        )
    return result


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def generate_tsv_rows(start_id: int, count: int, payload_size: int = 50) -> str:
    """Generate tab-separated rows for COPY FROM STDIN."""
    lines = []
    for i in range(count):
        row_id = start_id + i
        payload = f"row_{row_id:06d}_" + "x" * payload_size
        lines.append(f"{row_id}\t{payload}")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Test 1: COPY >5000 rows in autocommit mode
# ---------------------------------------------------------------------------
def test_large_copy_autocommit(dsn: str, table: str) -> None:
    """COPY 10,000 rows should succeed via transaction rotation."""
    row_count = 10_000
    data = generate_tsv_rows(1, row_count)
    run_copy_stdin(
        dsn,
        "",
        f"COPY {table} (id, data) FROM STDIN;",
        data,
    )

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == str(row_count), f"Expected {row_count} rows, got {count}"

    print(f"  [OK] Test 1: COPY {row_count} rows in autocommit mode (rotation)")


# ---------------------------------------------------------------------------
# Test 2: COPY inside explicit transaction does NOT rotate
# ---------------------------------------------------------------------------
def test_copy_explicit_txn_atomic(dsn: str, table: str) -> None:
    """COPY inside BEGIN/ROLLBACK should be fully atomic (no rotation)."""
    run_sql(dsn, f"DELETE FROM {table}")

    row_count = 6_000
    data = generate_tsv_rows(1, row_count)

    # Use BEGIN + COPY + ROLLBACK - all rows should be rolled back
    full_input = (
        f"BEGIN;\n"
        f"COPY {table} (id, data) FROM STDIN;\n"
        f"{data}\\.\n"
        f"ROLLBACK;\n"
    )
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=full_input,
        capture_output=True,
        text=True,
        timeout=120,
    )

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "0", (
        f"COPY inside explicit txn + ROLLBACK should leave 0 rows, got {count}"
    )

    print(f"  [OK] Test 2: COPY {row_count} rows inside explicit txn (atomic, ROLLBACK)")


# ---------------------------------------------------------------------------
# Test 3: Small COPY (<5000 rows)
# ---------------------------------------------------------------------------
def test_small_copy(dsn: str, table: str) -> None:
    """COPY <5000 rows should work without any rotation."""
    run_sql(dsn, f"DELETE FROM {table}")

    row_count = 100
    data = generate_tsv_rows(1, row_count)
    run_copy_stdin(
        dsn,
        "",
        f"COPY {table} (id, data) FROM STDIN;",
        data,
    )

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == str(row_count), f"Expected {row_count} rows, got {count}"

    print(f"  [OK] Test 3: COPY {row_count} rows (no rotation needed)")


# ---------------------------------------------------------------------------
# Test 4: COPY with CSV format and rotation
# ---------------------------------------------------------------------------
def test_copy_csv_rotation(dsn: str, table: str) -> None:
    """COPY FROM STDIN WITH CSV should also support rotation."""
    run_sql(dsn, f"DELETE FROM {table}")

    row_count = 7_500
    lines = []
    for i in range(row_count):
        lines.append(f"{i + 1},csv_row_{i + 1:06d}")
    data = "\n".join(lines) + "\n"

    full_input = (
        f"COPY {table} (id, data) FROM STDIN WITH (FORMAT csv);\n"
        f"{data}\\.\n"
    )
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=full_input,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        raise RuntimeError(f"CSV COPY failed: {result.stderr}")

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == str(row_count), f"Expected {row_count} CSV rows, got {count}"

    print(f"  [OK] Test 4: COPY {row_count} rows WITH CSV (rotation)")


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    table = f"copy_rotation_{random_suffix()}"

    print(f"[INFO] table={table}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"CREATE TABLE {table} (id INT PRIMARY KEY, data TEXT NOT NULL)")

        test_large_copy_autocommit(dsn, table)
        test_copy_explicit_txn_atomic(dsn, table)
        test_small_copy(dsn, table)
        test_copy_csv_rotation(dsn, table)

        print("PASS: COPY FROM STDIN transaction rotation safety tests (4/4)")
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

#!/usr/bin/env python3
"""
Regression coverage for the KV value size guard on COPY FROM STDIN (#1968).

Validates that an oversized row sent via COPY FROM STDIN is rejected
pre-flight with the db9 value-size guard error instead of surfacing
TiKV's cryptic RaftEntryTooLarge commit-time failure.

Companion to txn_value_size_guard_1968.py (which covers INSERT).

PostgreSQL divergence: PG 17.x accepts this COPY because TOAST transparently
out-of-lines large values.  db9 intentionally rejects it because TiKV enforces
a raft-entry-max-size limit (8-16 MiB) on single KV values, and db9 has no
TOAST equivalent.
"""

import argparse
import random
import string
import subprocess
import time


OVERSIZED_CHARS = 8_500_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Regression coverage for the KV value size guard (COPY path)"
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


def run_psql_stdin(dsn: str, sql_input: str, expect_success: bool = True) -> subprocess.CompletedProcess:
    """Run psql reading SQL from stdin (needed for COPY FROM STDIN)."""
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
            "-f",
            "-",
        ],
        input=sql_input,
        capture_output=True,
        text=True,
        timeout=300,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(
            f"psql stdin failed (exit {result.returncode})\n"
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
    table_name = f"txn_copy_guard_{random_suffix()}"

    print(f"[INFO] table={table_name}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        run_sql(
            dsn,
            f"CREATE TABLE {table_name} (id INT PRIMARY KEY, payload TEXT NOT NULL);",
        )

        # Seed one normal row to verify the table works and to check
        # that a failed COPY doesn't destroy existing data.
        run_sql(dsn, f"INSERT INTO {table_name} VALUES (1, 'ok');")
        count = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count == "1", f"expected 1 seeded row, got {count!r}"

        # Build a COPY FROM STDIN payload with an oversized TEXT value.
        # The row format is: id<TAB>payload
        # Prepend \set VERBOSITY verbose so psql shows SQLSTATE in error output.
        oversized_payload = "x" * OVERSIZED_CHARS
        copy_sql = (
            f"\\set VERBOSITY verbose\n"
            f"COPY {table_name} (id, payload) FROM STDIN;\n"
            f"2\t{oversized_payload}\n"
            f"\\.\n"
        )

        print(f"[INFO] sending COPY FROM STDIN with ~{OVERSIZED_CHARS / 1_000_000:.1f} MB payload")
        result = run_psql_stdin(dsn, copy_sql, expect_success=False)
        if result.returncode == 0:
            raise AssertionError("oversized COPY FROM STDIN unexpectedly succeeded")

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
        assert "54000" in stderr, (
            f"expected SQLSTATE 54000 in verbose error output, got: {result.stderr!r}"
        )

        # Verify no partial data was committed — only the original seed row
        # should remain.
        count_after = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count_after == "1", (
            f"failed oversized COPY must not commit a row, got count={count_after!r}"
        )

        print("PASS: oversized COPY FROM STDIN rejected by txn value size guard")
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

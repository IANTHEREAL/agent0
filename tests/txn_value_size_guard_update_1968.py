#!/usr/bin/env python3
"""
Regression coverage for the KV value size guard on UPDATE (#1968 / PR #1979).

Validates that an oversized row UPDATE is rejected pre-flight with the new,
actionable db9 error instead of surfacing TiKV's cryptic RaftEntryTooLarge
commit-time failure.  Complements txn_value_size_guard_1968.py which covers
INSERT.

PostgreSQL divergence: PG 17.x accepts this UPDATE because TOAST transparently
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
        description="Regression coverage for the KV value size guard (UPDATE path)"
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
    table_name = f"txn_value_guard_upd_{random_suffix()}"

    print(f"[INFO] table={table_name}")

    try:
        # ── Setup ──────────────────────────────────────────────────────
        run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        run_sql(
            dsn,
            f"CREATE TABLE {table_name} (id INT PRIMARY KEY, payload TEXT NOT NULL);",
        )

        # Seed a small row that we will attempt to UPDATE with an oversized value.
        run_sql(dsn, f"INSERT INTO {table_name} VALUES (1, 'small');")
        count = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count == "1", f"expected 1 seeded row, got {count!r}"

        # ── Oversized UPDATE ───────────────────────────────────────────
        # Use VERBOSITY verbose via stdin so psql shows SQLSTATE in output.
        oversized_update = (
            f"\\set VERBOSITY verbose\n"
            f"UPDATE {table_name} SET payload = repeat('x', {OVERSIZED_CHARS}) WHERE id = 1;\n"
        )
        result = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
             "-v", "ON_ERROR_STOP=1", "-f", "-"],
            input=oversized_update,
            capture_output=True, text=True, timeout=180,
        )
        if result.returncode == 0:
            raise AssertionError("oversized update unexpectedly succeeded")

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

        # ── Verify original row is unchanged ───────────────────────────
        payload_after = run_sql(dsn, f"SELECT payload FROM {table_name} WHERE id = 1;")
        assert payload_after == "small", (
            f"failed oversized update must not alter the row, got payload={payload_after!r}"
        )

        count_after = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count_after == "1", (
            f"row count must remain 1 after failed update, got count={count_after!r}"
        )

        print("PASS: oversized row update rejected by txn value size guard")
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

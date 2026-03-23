#!/usr/bin/env python3
"""
Regression coverage for the KV value size guard (#1968) — multi-column variant.

Validates that the guard fires when no single column exceeds the 8 MiB limit
but the total encoded row does.  Each of the three TEXT columns carries ~3 MB,
so the combined row (~9 MB) exceeds the default 8 MiB threshold.

PostgreSQL divergence: PG 17.x accepts this INSERT because TOAST transparently
out-of-lines large column values.  db9 intentionally rejects it because TiKV
enforces a raft-entry-max-size limit (8-16 MiB) on single KV values, and db9
has no TOAST equivalent.
"""

import argparse
import random
import string
import subprocess
import time


# Each column ~3 MB; 3 columns total ≈ 9 MB, exceeds the 8 MiB guard.
PER_COLUMN_CHARS = 3_000_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Multi-column regression coverage for the KV value size guard"
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
    table_name = f"txn_vguard_multi_{random_suffix()}"

    print(f"[INFO] table={table_name}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table_name};")
        run_sql(
            dsn,
            f"CREATE TABLE {table_name} ("
            f"  id INT PRIMARY KEY,"
            f"  col_a TEXT NOT NULL,"
            f"  col_b TEXT NOT NULL,"
            f"  col_c TEXT NOT NULL"
            f");",
        )

        # ── Sanity: a small row succeeds ────────────────────────────
        run_sql(dsn, f"INSERT INTO {table_name} VALUES (1, 'a', 'b', 'c');")
        count = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count == "1", f"expected 1 seeded row, got {count!r}"

        # ── Oversized multi-column INSERT ───────────────────────────
        # Each column: 3 000 000 bytes  ×  3 columns = 9 000 000 bytes.
        # This exceeds the 8 MiB (8 388 608 bytes) default guard limit
        # even though no single column alone would trigger it.
        # Use VERBOSITY verbose via stdin so psql shows SQLSTATE in output.
        oversized_insert = (
            f"\\set VERBOSITY verbose\n"
            f"INSERT INTO {table_name} VALUES ("
            f"  2,"
            f"  repeat('x', {PER_COLUMN_CHARS}),"
            f"  repeat('y', {PER_COLUMN_CHARS}),"
            f"  repeat('z', {PER_COLUMN_CHARS})"
            f");\n"
        )
        result = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
             "-v", "ON_ERROR_STOP=1", "-f", "-"],
            input=oversized_insert,
            capture_output=True, text=True, timeout=180,
        )
        if result.returncode == 0:
            raise AssertionError("oversized multi-column insert unexpectedly succeeded")

        stderr = (result.stderr or "").lower()
        assert "value too large" in stderr, (
            f"expected guard error, got: {result.stderr!r}"
        )
        assert "table row" in stderr, (
            f"expected subsystem hint, got: {result.stderr!r}"
        )
        assert "db9_txn_value_size_limit_bytes" in stderr, (
            f"expected env-var hint, got: {result.stderr!r}"
        )
        assert "raftentrytoolarge" not in stderr, (
            "expected pre-flight db9 guard, not TiKV commit-time raft error: "
            f"{result.stderr!r}"
        )
        assert "54000" in stderr, (
            f"expected SQLSTATE 54000 in verbose error output, got: {result.stderr!r}"
        )

        # ── Verify no row was committed ─────────────────────────────
        count_after = run_sql(dsn, f"SELECT COUNT(*) FROM {table_name};")
        assert count_after == "1", (
            f"failed oversized insert must not commit a row, got count={count_after!r}"
        )

        print("PASS: oversized multi-column row insert rejected by txn value size guard")
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

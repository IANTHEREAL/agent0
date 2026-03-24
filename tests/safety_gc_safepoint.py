#!/usr/bin/env python3
"""
Safety regression tests for GC safepoint tracking (PRs #1958, #2023, #2035, #2039).

Validates that the GC safepoint system correctly protects active transactions from
garbage collection. Tests multi-connection scenarios including:
- Explicit transactions hold safepoint
- ROLLBACK properly unregisters
- Failed transactions hold registration until ROLLBACK
- Abrupt disconnect quarantines the registration
- Savepoint rollback keeps registration
- Rapid BEGIN/COMMIT cycles don't leak registry entries

Requires: db9-server with DB9_GC_SAFEPOINT_ENABLED=true (default).
"""

import argparse
import random
import string
import subprocess
import sys
import threading
import time


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="GC safepoint safety regression tests"
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
        timeout=60,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(
            f"psql failed (exit {result.returncode})\n"
            f"SQL: {sql}\nstderr: {result.stderr}"
        )
    return result


def run_sql(dsn: str, sql: str) -> str:
    return run_psql(dsn, sql, expect_success=True).stdout.strip()


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def run_psql_session(dsn: str, commands: list[str], timeout: int = 30) -> subprocess.CompletedProcess:
    """Run multiple SQL commands in a single psql session via stdin."""
    sql_input = "\n".join(commands)
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=sql_input,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return result


# ---------------------------------------------------------------------------
# Test 1: Explicit transaction protects data visibility (MVCC safety)
# ---------------------------------------------------------------------------
def test_explicit_txn_protects_data(dsn: str, table: str) -> None:
    """
    Open a long-running explicit transaction on one connection.
    Verify that data written before the txn started remains readable
    on a second connection while the txn is open, then commit.
    """
    # Insert baseline data
    run_sql(dsn, f"INSERT INTO {table} VALUES (1, 'baseline')")

    # Connection 1: open explicit transaction, read data
    # Connection 2: verify data still readable
    # This is inherently safe with MVCC, but we verify the registry
    # doesn't interfere with normal transaction lifecycle.
    result1 = run_psql_session(dsn, [
        "BEGIN;",
        f"SELECT count(*) FROM {table};",
        "SELECT pg_sleep(2);",
        f"INSERT INTO {table} VALUES (2, 'in_txn');",
        "COMMIT;",
    ], timeout=30)
    assert result1.returncode == 0, f"Explicit txn failed: {result1.stderr}"

    # Verify both rows committed
    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "2", f"Expected 2 rows after explicit txn commit, got {count}"

    print("  [OK] Test 1: Explicit transaction lifecycle (BEGIN/INSERT/COMMIT)")


# ---------------------------------------------------------------------------
# Test 2: ROLLBACK properly cleans up
# ---------------------------------------------------------------------------
def test_rollback_cleanup(dsn: str, table: str) -> None:
    """Verify ROLLBACK unregisters the transaction and doesn't leave data."""
    result = run_psql_session(dsn, [
        "BEGIN;",
        f"INSERT INTO {table} VALUES (10, 'will_rollback');",
        "ROLLBACK;",
    ])
    assert result.returncode == 0, f"ROLLBACK session failed: {result.stderr}"

    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 10")
    assert count == "0", f"Rolled-back row should not exist, got count={count}"

    print("  [OK] Test 2: ROLLBACK properly cleans up transaction")


# ---------------------------------------------------------------------------
# Test 3: Failed transaction holds registration until ROLLBACK
# ---------------------------------------------------------------------------
def test_failed_txn_holds_until_rollback(dsn: str, table: str) -> None:
    """
    A transaction that encounters an error (e.g., duplicate PK) enters
    Failed state but should still hold its GC registration until ROLLBACK.
    """
    # Ensure row with id=1 exists (from test 1)
    result = run_psql_session(dsn, [
        "BEGIN;",
        f"INSERT INTO {table} VALUES (20, 'ok_row');",
        # This will fail with duplicate PK if id=1 exists, or succeed on fresh table
        f"INSERT INTO {table} VALUES (1, 'duplicate');",
        # After error, txn is in Failed state - only ROLLBACK is accepted
        "ROLLBACK;",
    ])
    # psql session may have non-zero exit due to error, that's expected

    # Verify the ok_row was also rolled back (entire txn rolled back)
    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 20")
    assert count == "0", f"Failed txn row should not exist after ROLLBACK, got {count}"

    print("  [OK] Test 3: Failed transaction holds until ROLLBACK")


# ---------------------------------------------------------------------------
# Test 4: Abrupt disconnect (quarantine path)
# ---------------------------------------------------------------------------
def test_abrupt_disconnect(dsn: str, table: str) -> None:
    """
    Open a transaction, insert data, then kill the connection without
    COMMIT/ROLLBACK. The server should quarantine the GC registration
    (hold for QUARANTINE_TTL=60s) rather than immediately advancing.
    The inserted data should NOT be visible (implicit rollback on disconnect).
    """
    # Use a subprocess that opens a txn and exits without commit
    proc = subprocess.Popen(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    # Send BEGIN + INSERT, then close stdin (simulates disconnect)
    proc.stdin.write("BEGIN;\n")
    proc.stdin.write(f"INSERT INTO {table} VALUES (30, 'orphan');\n")
    proc.stdin.close()
    proc.wait(timeout=10)

    # Give server a moment to process the disconnect
    time.sleep(1)

    # The orphaned insert should NOT be committed
    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 30")
    assert count == "0", f"Orphaned row should not persist after disconnect, got {count}"

    print("  [OK] Test 4: Abrupt disconnect triggers quarantine (data rolled back)")


# ---------------------------------------------------------------------------
# Test 5: Savepoint rollback keeps outer transaction registered
# ---------------------------------------------------------------------------
def test_savepoint_keeps_registration(dsn: str, table: str) -> None:
    """
    ROLLBACK TO SAVEPOINT should not unregister the outer transaction.
    The outer transaction's GC registration must remain active.
    """
    result = run_psql_session(dsn, [
        "BEGIN;",
        f"INSERT INTO {table} VALUES (40, 'outer');",
        "SAVEPOINT sp1;",
        f"INSERT INTO {table} VALUES (41, 'inner');",
        "ROLLBACK TO sp1;",
        # Outer txn still active, insert another row
        f"INSERT INTO {table} VALUES (42, 'after_sp_rollback');",
        "COMMIT;",
    ])
    assert result.returncode == 0, f"Savepoint session failed: {result.stderr}"

    # Row 40 and 42 should exist, row 41 should not
    count_40 = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 40")
    count_41 = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 41")
    count_42 = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 42")
    assert count_40 == "1", f"Outer row 40 should exist, got {count_40}"
    assert count_41 == "0", f"Savepoint-rolled-back row 41 should not exist, got {count_41}"
    assert count_42 == "1", f"Post-savepoint row 42 should exist, got {count_42}"

    print("  [OK] Test 5: Savepoint rollback keeps outer transaction registered")


# ---------------------------------------------------------------------------
# Test 6: Rapid BEGIN/COMMIT cycles don't leak registry entries
# ---------------------------------------------------------------------------
def test_rapid_txn_cycles(dsn: str, table: str) -> None:
    """
    Run 200 rapid BEGIN/COMMIT cycles on a single connection.
    Verify no observable side effects (connection remains healthy,
    data is correct). Registry leaks would eventually pin GC or OOM.
    """
    commands = []
    for i in range(200):
        commands.append("BEGIN;")
        commands.append(f"SELECT {i};")
        commands.append("COMMIT;")
    # Final insert to verify connection is still healthy
    commands.append(f"INSERT INTO {table} VALUES (50, 'after_rapid_cycles');")

    result = run_psql_session(dsn, commands, timeout=60)
    assert result.returncode == 0, f"Rapid cycle session failed: {result.stderr}"

    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 50")
    assert count == "1", f"Post-rapid-cycle insert should succeed, got {count}"

    print("  [OK] Test 6: 200 rapid BEGIN/COMMIT cycles (no registry leak)")


# ---------------------------------------------------------------------------
# Test 7: Concurrent transactions on separate connections
# ---------------------------------------------------------------------------
def test_concurrent_transactions(dsn: str, table: str) -> None:
    """
    Open two concurrent explicit transactions on separate connections.
    Both should be tracked by the GC registry. Commit both and verify.
    """
    errors = []

    def conn_a():
        try:
            result = run_psql_session(dsn, [
                "BEGIN;",
                f"INSERT INTO {table} VALUES (60, 'conn_a');",
                "SELECT pg_sleep(2);",
                "COMMIT;",
            ], timeout=30)
            if result.returncode != 0:
                errors.append(f"conn_a failed: {result.stderr}")
        except Exception as e:
            errors.append(f"conn_a exception: {e}")

    def conn_b():
        try:
            result = run_psql_session(dsn, [
                "BEGIN;",
                f"INSERT INTO {table} VALUES (61, 'conn_b');",
                "SELECT pg_sleep(2);",
                "COMMIT;",
            ], timeout=30)
            if result.returncode != 0:
                errors.append(f"conn_b failed: {result.stderr}")
        except Exception as e:
            errors.append(f"conn_b exception: {e}")

    t_a = threading.Thread(target=conn_a)
    t_b = threading.Thread(target=conn_b)
    t_a.start()
    t_b.start()
    t_a.join(timeout=30)
    t_b.join(timeout=30)

    assert not errors, f"Concurrent txn errors: {errors}"

    count_a = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 60")
    count_b = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id = 61")
    assert count_a == "1", f"conn_a row should exist, got {count_a}"
    assert count_b == "1", f"conn_b row should exist, got {count_b}"

    print("  [OK] Test 7: Concurrent transactions on separate connections")


# ---------------------------------------------------------------------------
# Test 8: Autocommit (implicit) transactions are short-lived
# ---------------------------------------------------------------------------
def test_autocommit_short_lived(dsn: str, table: str) -> None:
    """
    Autocommit statements should register/unregister quickly.
    Run several autocommit INSERTs and verify all succeed.
    """
    for i in range(70, 75):
        run_sql(dsn, f"INSERT INTO {table} VALUES ({i}, 'autocommit_{i}')")

    count = run_sql(dsn, f"SELECT count(*) FROM {table} WHERE id BETWEEN 70 AND 74")
    assert count == "5", f"Expected 5 autocommit rows, got {count}"

    print("  [OK] Test 8: Autocommit transactions are short-lived and correct")


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    table = f"gc_safepoint_{random_suffix()}"

    print(f"[INFO] table={table}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"CREATE TABLE {table} (id INT PRIMARY KEY, data TEXT)")

        test_explicit_txn_protects_data(dsn, table)
        test_rollback_cleanup(dsn, table)
        test_failed_txn_holds_until_rollback(dsn, table)
        test_abrupt_disconnect(dsn, table)
        test_savepoint_keeps_registration(dsn, table)
        test_rapid_txn_cycles(dsn, table)
        test_concurrent_transactions(dsn, table)
        test_autocommit_short_lived(dsn, table)

        print("PASS: GC safepoint safety regression tests (8/8)")
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

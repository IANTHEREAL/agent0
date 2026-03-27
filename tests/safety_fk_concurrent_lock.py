#!/usr/bin/env python3
"""
Concurrent FK validation test: verify that FK parent-row locking prevents
orphaned child rows.

Scenario:
  1. Table `parent(id PK)` with row id=1
  2. Table `child(id PK, parent_id FK -> parent.id)`
  3. Txn A: BEGIN; INSERT INTO child (id, parent_id) VALUES (10, 1);
     -- FK check succeeds AND locks parent row id=1
  4. Txn B: BEGIN; DELETE FROM parent WHERE id = 1;
     -- Must block (waiting for Txn A's lock) or get write conflict
  5. Txn A: COMMIT;
  6. Txn B: should fail with FK violation (CASCADE would delete child)
     or with a serialization/write conflict error.

Expected: the concurrent DELETE either blocks until Txn A commits (then
sees the child row and fails with FK restriction), or gets a write
conflict.  The child row must NEVER reference a deleted parent.
"""

import argparse
import os
import random
import subprocess
import sys
import threading
import time


def parse_args():
    parser = argparse.ArgumentParser(description="FK concurrent lock test")
    parser.add_argument("--dsn", required=True)
    return parser.parse_args()


def run_sql(dsn, sql, timeout=30):
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )
    return result


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000, 9999)}"


def test_fk_concurrent_insert_delete(dsn):
    """
    Core test: INSERT child + DELETE parent concurrently.
    The FK lock on the parent row should prevent the delete from
    succeeding while the child INSERT is in-flight.
    """
    sfx = random_suffix()
    parent = f"fk_lock_parent_{sfx}"
    child = f"fk_lock_child_{sfx}"

    # Setup
    run_sql(dsn, f"DROP TABLE IF EXISTS {child} CASCADE")
    run_sql(dsn, f"DROP TABLE IF EXISTS {parent} CASCADE")
    run_sql(dsn, f"CREATE TABLE {parent} (id INT PRIMARY KEY)")
    run_sql(dsn, f"""
        CREATE TABLE {child} (
            id INT PRIMARY KEY,
            parent_id INT NOT NULL REFERENCES {parent}(id)
        )
    """)
    run_sql(dsn, f"INSERT INTO {parent} (id) VALUES (1), (2), (3)")

    # Use psql's multi-statement mode via -f - to run transactions
    # Txn A: insert child referencing parent id=1, hold open
    # Txn B: try to delete parent id=1 concurrently

    txn_a_result = {"returncode": None, "stdout": "", "stderr": ""}
    txn_b_result = {"returncode": None, "stdout": "", "stderr": ""}

    def txn_a():
        """Insert child row referencing parent id=1 in an explicit txn."""
        sql = f"""
BEGIN;
INSERT INTO {child} (id, parent_id) VALUES (10, 1);
-- Hold the transaction open for a moment so Txn B can attempt DELETE
SELECT pg_sleep(2);
COMMIT;
"""
        r = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            input=sql, capture_output=True, text=True, timeout=30,
        )
        txn_a_result["returncode"] = r.returncode
        txn_a_result["stdout"] = r.stdout
        txn_a_result["stderr"] = r.stderr

    def txn_b():
        """Wait briefly, then try to delete the parent row."""
        time.sleep(0.5)  # Let Txn A start first
        sql = f"""
BEGIN;
DELETE FROM {parent} WHERE id = 1;
COMMIT;
"""
        r = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            input=sql, capture_output=True, text=True, timeout=30,
        )
        txn_b_result["returncode"] = r.returncode
        txn_b_result["stdout"] = r.stdout
        txn_b_result["stderr"] = r.stderr

    t_a = threading.Thread(target=txn_a)
    t_b = threading.Thread(target=txn_b)
    t_a.start()
    t_b.start()
    t_a.join(timeout=30)
    t_b.join(timeout=30)

    # Verify final state
    # The child row (10, 1) MUST exist
    child_check = run_sql(dsn, f"SELECT count(*) FROM {child} WHERE id = 10 AND parent_id = 1")
    child_exists = child_check.stdout.strip() == "1"

    # The parent row id=1 MUST still exist (delete should have failed)
    parent_check = run_sql(dsn, f"SELECT count(*) FROM {parent} WHERE id = 1")
    parent_exists = parent_check.stdout.strip() == "1"

    # Txn A should succeed
    txn_a_ok = txn_a_result["returncode"] == 0

    # Txn B should fail (FK restriction or write conflict)
    txn_b_failed = txn_b_result["returncode"] != 0

    print(f"\n  Txn A (INSERT child): rc={txn_a_result['returncode']}")
    if txn_a_result["stderr"]:
        print(f"    stderr: {txn_a_result['stderr'][:200]}")
    print(f"  Txn B (DELETE parent): rc={txn_b_result['returncode']}")
    if txn_b_result["stderr"]:
        print(f"    stderr: {txn_b_result['stderr'][:200]}")
    print(f"  Child row exists: {child_exists}")
    print(f"  Parent row exists: {parent_exists}")

    if child_exists and parent_exists and txn_a_ok:
        if txn_b_failed:
            print("  [PASS] FK lock prevented concurrent parent deletion (Txn B failed)")
            status = "PASS"
        else:
            # Txn B succeeded but parent still exists — means DELETE was
            # blocked until after commit, then ON DELETE RESTRICT kicked in
            print("  [PASS] FK lock blocked concurrent parent deletion")
            status = "PASS"
    elif child_exists and not parent_exists:
        print("  [BUG] ORPHANED CHILD ROW — parent deleted while child references it!")
        status = "BUG"
    else:
        print(f"  [FAIL] Unexpected state: child_exists={child_exists}, "
              f"parent_exists={parent_exists}, txn_a_ok={txn_a_ok}")
        status = "FAIL"

    # Cleanup
    run_sql(dsn, f"DROP TABLE IF EXISTS {child} CASCADE")
    run_sql(dsn, f"DROP TABLE IF EXISTS {parent} CASCADE")

    return status


def test_fk_concurrent_unique_index_ref(dsn):
    """
    Same test but FK references a UNIQUE column (not PK) to exercise
    the UniqueIndex lookup path.
    """
    sfx = random_suffix()
    parent = f"fk_uidx_parent_{sfx}"
    child = f"fk_uidx_child_{sfx}"

    run_sql(dsn, f"DROP TABLE IF EXISTS {child} CASCADE")
    run_sql(dsn, f"DROP TABLE IF EXISTS {parent} CASCADE")
    run_sql(dsn, f"""
        CREATE TABLE {parent} (
            id INT PRIMARY KEY,
            code TEXT NOT NULL UNIQUE
        )
    """)
    run_sql(dsn, f"""
        CREATE TABLE {child} (
            id INT PRIMARY KEY,
            parent_code TEXT NOT NULL REFERENCES {parent}(code)
        )
    """)
    run_sql(dsn, f"INSERT INTO {parent} (id, code) VALUES (1, 'alpha'), (2, 'beta')")

    txn_a_result = {"returncode": None, "stderr": ""}
    txn_b_result = {"returncode": None, "stderr": ""}

    def txn_a():
        sql = f"""
BEGIN;
INSERT INTO {child} (id, parent_code) VALUES (10, 'alpha');
SELECT pg_sleep(2);
COMMIT;
"""
        r = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            input=sql, capture_output=True, text=True, timeout=30,
        )
        txn_a_result["returncode"] = r.returncode
        txn_a_result["stderr"] = r.stderr

    def txn_b():
        time.sleep(0.5)
        sql = f"""
BEGIN;
DELETE FROM {parent} WHERE code = 'alpha';
COMMIT;
"""
        r = subprocess.run(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            input=sql, capture_output=True, text=True, timeout=30,
        )
        txn_b_result["returncode"] = r.returncode
        txn_b_result["stderr"] = r.stderr

    t_a = threading.Thread(target=txn_a)
    t_b = threading.Thread(target=txn_b)
    t_a.start()
    t_b.start()
    t_a.join(timeout=30)
    t_b.join(timeout=30)

    child_check = run_sql(dsn, f"SELECT count(*) FROM {child} WHERE parent_code = 'alpha'")
    child_exists = child_check.stdout.strip() == "1"
    parent_check = run_sql(dsn, f"SELECT count(*) FROM {parent} WHERE code = 'alpha'")
    parent_exists = parent_check.stdout.strip() == "1"

    print(f"\n  Txn A (INSERT child via unique idx): rc={txn_a_result['returncode']}")
    print(f"  Txn B (DELETE parent): rc={txn_b_result['returncode']}")
    print(f"  Child row exists: {child_exists}")
    print(f"  Parent row exists: {parent_exists}")

    if child_exists and parent_exists:
        print("  [PASS] FK lock on unique-index ref prevented orphan")
        status = "PASS"
    elif child_exists and not parent_exists:
        print("  [BUG] ORPHANED CHILD ROW via unique-index FK path!")
        status = "BUG"
    else:
        print(f"  [FAIL] Unexpected state")
        status = "FAIL"

    run_sql(dsn, f"DROP TABLE IF EXISTS {child} CASCADE")
    run_sql(dsn, f"DROP TABLE IF EXISTS {parent} CASCADE")
    return status


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("FK Concurrent Lock Safety Test")
    print("=" * 60)

    results = []

    print("\n1. FK lock: concurrent INSERT child + DELETE parent (PK ref)")
    results.append(("pk_ref", test_fk_concurrent_insert_delete(dsn)))

    print("\n2. FK lock: concurrent INSERT child + DELETE parent (unique index ref)")
    results.append(("unique_idx_ref", test_fk_concurrent_unique_index_ref(dsn)))

    print("\n" + "=" * 60)
    print("Summary:")
    bugs = sum(1 for _, s in results if s == "BUG")
    fails = sum(1 for _, s in results if s == "FAIL")
    passes = sum(1 for _, s in results if s == "PASS")
    print(f"  PASS: {passes}  FAIL: {fails}  BUG: {bugs}")

    if bugs > 0:
        print("\n  *** BUGS FOUND — FK parent-row locking is not working ***")
        sys.exit(2)
    elif fails > 0:
        print("\n  *** FAILURES — check test output ***")
        sys.exit(1)
    else:
        print("\n  All tests passed.")
        sys.exit(0)


if __name__ == "__main__":
    main()

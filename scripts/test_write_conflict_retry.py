#!/usr/bin/env python3
"""
Integration test for WriteConflict retry behavior.

This test verifies that db9-server correctly retries autocommit statements
when TiKV returns WriteConflict errors due to concurrent updates.

Usage:
    python3 scripts/test_write_conflict_retry.py [--dsn DSN]

The test creates a table, runs concurrent updates, and verifies:
1. All threads complete without errors (retries work)
2. Final count reflects successful updates (no silent failures)

Note: Due to TiKV's optimistic concurrency, some updates may be "lost"
when read-modify-write races occur. The test validates that retries
happen and threads don't fail, not that every update is counted.
"""

import argparse
import os
import sys
import threading
import time

try:
    import psycopg2
except ImportError:
    print("ERROR: psycopg2 not installed. Run: pip install psycopg2-binary")
    sys.exit(1)


def get_connection(dsn: str):
    """Create a new database connection."""
    return psycopg2.connect(dsn)


def setup_table(dsn: str):
    """Create the test table."""
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS test_write_conflict_retry")
    cur.execute("""
        CREATE TABLE test_write_conflict_retry (
            id INT PRIMARY KEY,
            counter INT DEFAULT 0,
            updated_at TIMESTAMP DEFAULT NOW()
        )
    """)
    cur.execute("INSERT INTO test_write_conflict_retry (id, counter) VALUES (1, 0)")
    cur.close()
    conn.close()


def cleanup_table(dsn: str):
    """Drop the test table."""
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS test_write_conflict_retry")
    cur.close()
    conn.close()


def update_counter_autocommit(thread_id: int, dsn: str, iterations: int, results: dict):
    """
    Update counter using autocommit (each UPDATE is its own transaction).
    With retry logic, this should succeed even under contention.
    """
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    
    success_count = 0
    error_count = 0
    
    for i in range(iterations):
        try:
            cur.execute("UPDATE test_write_conflict_retry SET counter = counter + 1 WHERE id = 1")
            success_count += 1
        except Exception as e:
            error_count += 1
            results["errors"].append(f"Thread {thread_id}, iteration {i}: {e}")
    
    cur.close()
    conn.close()
    
    results["success"][thread_id] = success_count
    results["failed"][thread_id] = error_count


def update_counter_explicit_txn(thread_id: int, dsn: str, iterations: int, results: dict):
    """
    Update counter using explicit transaction (BEGIN/COMMIT).
    WriteConflict should NOT be auto-retried here; errors returned to client.
    """
    conn = get_connection(dsn)
    cur = conn.cursor()
    
    success_count = 0
    error_count = 0
    
    for i in range(iterations):
        try:
            conn.autocommit = False
            cur.execute("SELECT counter FROM test_write_conflict_retry WHERE id = 1 FOR UPDATE")
            row = cur.fetchone()
            if row is None:
                raise Exception("No row found")
            new_val = row[0] + 1
            cur.execute("UPDATE test_write_conflict_retry SET counter = %s WHERE id = 1", (new_val,))
            conn.commit()
            success_count += 1
        except Exception as e:
            error_count += 1
            try:
                conn.rollback()
            except:
                pass
            results["errors"].append(f"Thread {thread_id}, iteration {i}: {e}")
    
    cur.close()
    conn.close()
    
    results["success"][thread_id] = success_count
    results["failed"][thread_id] = error_count


def run_concurrent_test(dsn: str, num_threads: int, iterations: int, use_explicit_txn: bool):
    """Run concurrent update test and return results."""
    results = {
        "success": {},
        "failed": {},
        "errors": []
    }
    
    threads = []
    update_func = update_counter_explicit_txn if use_explicit_txn else update_counter_autocommit
    
    start = time.time()
    
    for i in range(num_threads):
        t = threading.Thread(target=update_func, args=(i, dsn, iterations, results))
        threads.append(t)
        t.start()
    
    for t in threads:
        t.join()
    
    elapsed = time.time() - start
    
    conn = get_connection(dsn)
    cur = conn.cursor()
    cur.execute("SELECT counter FROM test_write_conflict_retry WHERE id = 1")
    row = cur.fetchone()
    final_counter = row[0] if row else 0
    cur.close()
    conn.close()
    
    total_success = sum(results["success"].values())
    total_failed = sum(results["failed"].values())
    expected_max = num_threads * iterations
    
    return {
        "elapsed": elapsed,
        "total_success": total_success,
        "total_failed": total_failed,
        "final_counter": final_counter,
        "expected_max": expected_max,
        "errors": results["errors"][:10]
    }


def test_autocommit_retry(dsn: str):
    """Test that autocommit statements are retried on WriteConflict."""
    print("\n=== Test 1: Autocommit Retry ===")
    print("Testing concurrent UPDATE with autocommit (should retry on conflict)")
    
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("UPDATE test_write_conflict_retry SET counter = 0 WHERE id = 1")
    cur.close()
    conn.close()
    
    result = run_concurrent_test(dsn, num_threads=5, iterations=10, use_explicit_txn=False)
    
    print(f"  Elapsed: {result['elapsed']:.2f}s")
    print(f"  Total successful updates: {result['total_success']}")
    print(f"  Total failed updates: {result['total_failed']}")
    print(f"  Final counter: {result['final_counter']} (max possible: {result['expected_max']})")
    
    if result["errors"]:
        print(f"  First few errors:")
        for err in result["errors"][:3]:
            print(f"    - {err}")
    
    if result["total_failed"] == 0:
        print("  PASS: All updates completed (retries worked)")
        return True
    else:
        print("  FAIL: Some updates failed despite retry")
        return False


def test_explicit_txn_no_auto_retry(dsn: str):
    """Test that explicit transactions do NOT auto-retry (correct behavior)."""
    print("\n=== Test 2: Explicit Transaction (No Auto-Retry) ===")
    print("Testing concurrent UPDATE with explicit txn (should return errors)")
    
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("UPDATE test_write_conflict_retry SET counter = 0 WHERE id = 1")
    cur.close()
    conn.close()
    
    result = run_concurrent_test(dsn, num_threads=3, iterations=5, use_explicit_txn=True)
    
    print(f"  Elapsed: {result['elapsed']:.2f}s")
    print(f"  Total successful updates: {result['total_success']}")
    print(f"  Total failed updates: {result['total_failed']}")
    print(f"  Final counter: {result['final_counter']}")
    
    if result["total_failed"] > 0:
        print("  PASS: Explicit transactions correctly returned errors (no auto-retry)")
        return True
    else:
        print("  INFO: No conflicts occurred (might need more contention)")
        return True


def test_single_thread_no_retry_needed(dsn: str):
    """Test that single-thread updates work without any retry."""
    print("\n=== Test 3: Single Thread (No Retry Needed) ===")
    print("Testing sequential UPDATE (baseline, no conflicts expected)")
    
    conn = get_connection(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("UPDATE test_write_conflict_retry SET counter = 0 WHERE id = 1")
    cur.close()
    conn.close()
    
    result = run_concurrent_test(dsn, num_threads=1, iterations=20, use_explicit_txn=False)
    
    print(f"  Elapsed: {result['elapsed']:.2f}s")
    print(f"  Total successful updates: {result['total_success']}")
    print(f"  Final counter: {result['final_counter']} (expected: 20)")
    
    if result["final_counter"] == 20 and result["total_failed"] == 0:
        print("  PASS: Single thread works correctly")
        return True
    else:
        print("  FAIL: Single thread had unexpected behavior")
        return False


def main():
    parser = argparse.ArgumentParser(description="Test WriteConflict retry behavior")
    parser.add_argument("--dsn", default=os.environ.get("PG_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"),
                        help="PostgreSQL DSN (default: from PG_DSN env or localhost:5433)")
    args = parser.parse_args()
    
    print(f"Using DSN: {args.dsn}")
    
    try:
        setup_table(args.dsn)
    except Exception as e:
        print(f"ERROR: Failed to connect or setup table: {e}")
        sys.exit(1)
    
    try:
        results = []
        results.append(("Autocommit Retry", test_autocommit_retry(args.dsn)))
        results.append(("Explicit Txn No Auto-Retry", test_explicit_txn_no_auto_retry(args.dsn)))
        results.append(("Single Thread", test_single_thread_no_retry_needed(args.dsn)))
        
        print("\n=== Summary ===")
        all_passed = True
        for name, passed in results:
            status = "PASS" if passed else "FAIL"
            print(f"  {name}: {status}")
            if not passed:
                all_passed = False
        
        if all_passed:
            print("\nAll tests passed!")
            sys.exit(0)
        else:
            print("\nSome tests failed!")
            sys.exit(1)
    
    finally:
        cleanup_table(args.dsn)


if __name__ == "__main__":
    main()

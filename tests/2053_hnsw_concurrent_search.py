#!/usr/bin/env python3
"""
PR #2053: HNSW shared index cache concurrent query correctness.

Verifies that N concurrent HNSW queries all return correct results
while sharing a single base graph via Arc<SharedHnswIndex>.

Tests:
1. 10 concurrent queries all return same correct nearest neighbor
2. After INSERT (delta), concurrent queries all see the new vector
3. No crashes or deadlocks under concurrent access
"""

import argparse
import random
import string
import subprocess
import threading
import time


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="HNSW shared cache concurrent query test"
    )
    parser.add_argument(
        "--dsn", required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def run_sql(dsn: str, sql: str) -> str:
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}")
    return result.stdout.strip()


def random_suffix() -> str:
    ts = int(time.time())
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=6))
    return f"{ts}_{rand}"


def hnsw_query(dsn: str, table: str, query_vec: str, results: list, idx: int) -> None:
    """Run an HNSW query and store the result."""
    try:
        result = run_sql(
            dsn,
            f"SELECT id FROM {table} ORDER BY v <-> '{query_vec}' LIMIT 1",
        )
        results[idx] = ("ok", result)
    except Exception as e:
        results[idx] = ("error", str(e))


def test_concurrent_same_query(dsn: str, table: str) -> None:
    """10 concurrent queries for the same vector should all return the same id."""
    n_threads = 10
    results = [None] * n_threads
    threads = []

    for i in range(n_threads):
        t = threading.Thread(
            target=hnsw_query,
            args=(dsn, table, "[1.0, 0.0, 0.0]", results, i),
        )
        threads.append(t)
        t.start()

    for t in threads:
        t.join(timeout=30)

    errors = [r for r in results if r is None or r[0] == "error"]
    assert not errors, f"Concurrent queries had errors: {errors}"

    ids = [r[1] for r in results]
    assert all(id_val == "1" for id_val in ids), (
        f"All concurrent queries should return id=1, got: {ids}"
    )

    print(f"  [OK] Test 1: {n_threads} concurrent queries all returned id=1")


def test_concurrent_after_insert(dsn: str, table: str) -> None:
    """After INSERT (delta), concurrent queries should all see the new vector."""
    # Insert a vector closer to [1,0,0] than id=1
    run_sql(dsn, f"INSERT INTO {table} (id, v) VALUES (100, '[0.99, 0.0, 0.0]')")

    n_threads = 10
    results = [None] * n_threads
    threads = []

    for i in range(n_threads):
        t = threading.Thread(
            target=hnsw_query,
            args=(dsn, table, "[1.0, 0.0, 0.0]", results, i),
        )
        threads.append(t)
        t.start()

    for t in threads:
        t.join(timeout=30)

    errors = [r for r in results if r is None or r[0] == "error"]
    assert not errors, f"Post-INSERT concurrent queries had errors: {errors}"

    ids = [r[1] for r in results]
    assert all(id_val == "100" for id_val in ids), (
        f"All concurrent queries should return id=100 (delta), got: {ids}"
    )

    print(f"  [OK] Test 2: {n_threads} concurrent queries all saw delta INSERT (id=100)")


def test_concurrent_mixed_queries(dsn: str, table: str) -> None:
    """Mixed queries for different vectors, all concurrent."""
    query_vecs = [
        ("[1.0, 0.0, 0.0]", "100"),  # closest to new insert
        ("[0.0, 1.0, 0.0]", "2"),
        ("[0.0, 0.0, 1.0]", "3"),
    ]

    n_per_query = 4
    total = len(query_vecs) * n_per_query
    results = [None] * total
    threads = []

    for qi, (vec, _) in enumerate(query_vecs):
        for j in range(n_per_query):
            idx = qi * n_per_query + j
            t = threading.Thread(
                target=hnsw_query,
                args=(dsn, table, vec, results, idx),
            )
            threads.append(t)
            t.start()

    for t in threads:
        t.join(timeout=30)

    errors = [r for r in results if r is None or r[0] == "error"]
    assert not errors, f"Mixed concurrent queries had errors: {errors}"

    for qi, (vec, expected_id) in enumerate(query_vecs):
        group_ids = [results[qi * n_per_query + j][1] for j in range(n_per_query)]
        assert all(id_val == expected_id for id_val in group_ids), (
            f"Query for {vec}: expected all id={expected_id}, got {group_ids}"
        )

    print(f"  [OK] Test 3: {total} mixed concurrent queries all correct")


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    table = f"hnsw_conc_{random_suffix()}"

    print(f"[INFO] table={table}")

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"""
            CREATE TABLE {table} (id INT PRIMARY KEY, v VECTOR(3))
        """)
        run_sql(dsn, f"""
            INSERT INTO {table} (id, v) VALUES
                (1, '[0.9, 0.0, 0.0]'),
                (2, '[0.0, 0.9, 0.0]'),
                (3, '[0.0, 0.0, 0.9]')
        """)
        run_sql(dsn, f"""
            CREATE INDEX idx_{table} ON {table} USING hnsw (v vector_l2_ops)
        """)

        # Wait for merge
        time.sleep(5)

        test_concurrent_same_query(dsn, table)
        test_concurrent_after_insert(dsn, table)
        test_concurrent_mixed_queries(dsn, table)

        print("PASS: HNSW shared cache concurrent search tests (3/3)")
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

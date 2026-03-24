#!/usr/bin/env python3
"""
HNSW delta budget truncation test (PR #2053).

When pending deltas exceed 10% of the index cache memory budget,
scan_visible_deltas truncates to the budget limit. The query still
returns results from the base graph + partial deltas, but the most
recent inserts beyond the budget may be invisible until merge.

For dim=3, m=16, default 2GB cache: budget ≈ 362K deltas.
We can't realistically create 362K rows in a test, so we test the
observable behavior: insert many rows without merge, verify query
still returns results (partial or full), and no crash/error.

For a tighter test, we use higher dimensions to reduce the budget.
dim=128, m=16: per_delta ≈ 128*4 + 32 + 128*4 + 16*8 + 40 = 1224 bytes
budget = 200MB / 1224 ≈ 163K deltas. Still too many for a quick test.

Practical approach: verify correctness with moderate data (5000 inserts
before merge) and verify the system doesn't crash or lose data.
"""

import argparse
import random
import subprocess
import time


def parse_args():
    parser = argparse.ArgumentParser(
        description="HNSW delta budget test"
    )
    parser.add_argument("--dsn", required=True)
    return parser.parse_args()


def run_sql(dsn, sql, timeout=120):
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}")
    return result.stdout.strip()


def run_psql(dsn, sql, expect_success=True, timeout=120):
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000, 9999)}"


RESULTS = []


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        for line in detail.split("\n"):
            print(f"        {line}")


def test_large_delta_backlog(dsn):
    """Insert 5000 vectors without merge, then query. All should be findable
    via delta-only search (no base graph yet or stale base graph)."""
    sfx = random_suffix()
    tbl = f"hnsw_delta_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(16))")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

        # Insert 5000 vectors in batches BEFORE merge can run
        n_rows = 5000
        batch_size = 100
        print(f"        inserting {n_rows} vectors (16d) in {n_rows // batch_size} batches...")
        for batch in range(n_rows // batch_size):
            vals = ", ".join(
                f"({batch * batch_size + i}, "
                f"'[{','.join(str(round(random.random(), 3)) for _ in range(16))}]')"
                for i in range(1, batch_size + 1)
            )
            run_sql(dsn, f"INSERT INTO {tbl} VALUES {vals}")

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        assert count == str(n_rows), f"insert failed: {count}"

        # Query immediately (all in delta, base graph empty or small)
        qv = "[" + ",".join(str(round(random.random(), 3)) for _ in range(16)) + "]"
        result = run_psql(dsn,
            f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 100) t",
            expect_success=False)

        if result.returncode == 0:
            result_count = int(result.stdout.strip())
            if result_count >= 1:
                report(f"HNSW delta backlog: {n_rows} deltas, query returns {result_count} results", "PASS",
                       "delta-only search working (may be partial if budget exceeded)")
            else:
                report("HNSW delta backlog: empty result", "BUG",
                       "query returned 0 results despite 5000 inserted rows")
        else:
            stderr = result.stderr.lower()
            if "delta" in stderr or "budget" in stderr:
                report("HNSW delta backlog: error on large delta", "BUG",
                       f"query should degrade gracefully, not error: {result.stderr[:200]}")
            else:
                report("HNSW delta backlog: query error", "FAIL", result.stderr[:200])

        # Verify exact count via sequential scan (no HNSW)
        seq_count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if seq_count == str(n_rows):
            report("HNSW delta backlog: sequential scan correct", "PASS")
        else:
            report("HNSW delta backlog: data loss", "BUG",
                   f"expected {n_rows}, got {seq_count}")

        # Wait for merge, then re-query
        print("        waiting 8s for merge...")
        time.sleep(8)

        result = run_sql(dsn,
            f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 100) t")
        post_merge_count = int(result)
        if post_merge_count >= 50:
            report(f"HNSW delta backlog: post-merge query returns {post_merge_count}", "PASS")
        else:
            report("HNSW delta backlog: post-merge result too few", "FAIL",
                   f"expected >= 50, got {post_merge_count}")

    except Exception as e:
        report("HNSW delta backlog", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_mixed_insert_update_delete_deltas(dsn):
    """Insert 1000, update 500, delete 200, then query.
    Delta index must handle all three types correctly."""
    sfx = random_suffix()
    tbl = f"hnsw_mixed_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

        # Insert 1000 rows
        for batch in range(10):
            vals = ", ".join(
                f"({batch * 100 + i}, '[{random.random():.3f},{random.random():.3f},{random.random():.3f}]')"
                for i in range(1, 101)
            )
            run_sql(dsn, f"INSERT INTO {tbl} VALUES {vals}")

        # Update 500 rows (new vectors)
        for batch in range(5):
            for i in range(100):
                rid = batch * 100 + i + 1
                run_sql(dsn,
                    f"UPDATE {tbl} SET v = '[{random.random():.3f},{random.random():.3f},{random.random():.3f}]' WHERE id = {rid}")

        # Delete 200 rows
        run_sql(dsn, f"DELETE FROM {tbl} WHERE id BETWEEN 801 AND 1000")

        # Query: should return results from remaining 800 rows
        qv = "[0.5, 0.5, 0.5]"
        result = run_sql(dsn,
            f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 50) t")
        count = int(result)

        # Verify via sequential scan
        seq_count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")

        if seq_count == "800":
            report("HNSW mixed I/U/D deltas: sequential count correct (800)", "PASS")
        else:
            report("HNSW mixed I/U/D deltas: wrong count", "BUG",
                   f"expected 800, got {seq_count}")

        if count >= 1:
            report(f"HNSW mixed I/U/D deltas: HNSW query returns {count}", "PASS")
        else:
            report("HNSW mixed I/U/D deltas: empty HNSW result", "BUG",
                   "query returned 0 results")

        # Verify no deleted rows appear in search
        deleted_check = run_sql(dsn,
            f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 1000) t "
            f"WHERE id BETWEEN 801 AND 1000")
        if deleted_check == "0":
            report("HNSW mixed I/U/D deltas: no ghost deleted rows", "PASS")
        else:
            report("HNSW mixed I/U/D deltas: ghost rows from deleted", "BUG",
                   f"{deleted_check} deleted rows appeared in search")

    except Exception as e:
        report("HNSW mixed deltas", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("HNSW DELTA BUDGET + MIXED DML TEST")
    print("=" * 60)

    test_large_delta_backlog(dsn)
    test_mixed_insert_update_delete_deltas(dsn)

    print("\n" + "=" * 60)
    bugs = [r for r in RESULTS if r[1] == "BUG"]
    fails = [r for r in RESULTS if r[1] == "FAIL"]
    passes = [r for r in RESULTS if r[1] == "PASS"]
    print(f"  PASS: {len(passes)}  FAIL: {len(fails)}  BUG: {len(bugs)}")

    if bugs:
        print("\n--- BUGS ---")
        for n, _, d in bugs:
            print(f"  [BUG] {n}: {d}")
    if fails:
        print("\n--- FAILURES ---")
        for n, _, d in fails:
            print(f"  [FAIL] {n}: {d}")

    return 1 if bugs else 0


if __name__ == "__main__":
    raise SystemExit(main())

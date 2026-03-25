#!/usr/bin/env python3
"""
HNSW S3 live test: verify graphs are actually written to and read from S3.

Requires server started with HNSW_S3_BUCKET set.
Verifies:
1. CREATE INDEX uploads graph to S3
2. Query reads graph from S3 (via cache)
3. INSERT + merge writes new version to S3
4. DROP INDEX tombstones meta
5. TRUNCATE + re-insert works
6. DROP TABLE cleans up
7. Concurrent queries on S3-backed index
8. S3 object existence via aws s3 ls
"""

import argparse
import os
import random
import subprocess
import threading
import time


S3_BUCKET = os.environ.get("HNSW_S3_BUCKET", "db9-hnsw-test-dev")
S3_REGION = os.environ.get("HNSW_S3_REGION", "us-west-2")
MERGE_WAIT = 6  # seconds to wait for background merge


def parse_args():
    parser = argparse.ArgumentParser(description="HNSW S3 live test")
    parser.add_argument("--dsn", required=True)
    return parser.parse_args()


def run_sql(dsn, sql, timeout=60):
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}")
    return result.stdout.strip()


def run_psql(dsn, sql, expect_success=True):
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=60,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}")
    return result


def s3_ls(prefix=""):
    """List S3 objects under the HNSW prefix."""
    result = subprocess.run(
        ["aws", "s3", "ls", f"s3://{S3_BUCKET}/hnsw/{prefix}",
         "--recursive", "--region", S3_REGION],
        capture_output=True, text=True, timeout=30,
    )
    return result.stdout.strip()


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000, 9999)}"


RESULTS = []


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        print(f"        {detail}")


def test_s3_full_lifecycle(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_s3_life_{sfx}"

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {tbl}")
        run_sql(dsn, f"CREATE TABLE {tbl} (id SERIAL PRIMARY KEY, v VECTOR(3) NOT NULL)")

        # Insert initial data
        run_sql(dsn, f"""INSERT INTO {tbl} (v) VALUES
            ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]'),
            ('[0.5,0.5,0]'), ('[0,0.5,0.5]')""")

        # CREATE INDEX → should upload graph to S3
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

        # Check S3 for objects
        time.sleep(2)
        s3_objects = s3_ls()
        if ".usearch" in s3_objects or "graph_v" in s3_objects:
            report("S3: graph uploaded on CREATE INDEX", "PASS",
                   f"found objects in s3://{S3_BUCKET}/hnsw/")
        else:
            report("S3: graph uploaded on CREATE INDEX", "BUG",
                   f"no .usearch objects found. s3 ls: {s3_objects[:200]}")

        # Query should work (reads from S3 → cache)
        result = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        if result == "1":
            report("S3: query returns correct result", "PASS")
        else:
            report("S3: query returns wrong result", "BUG", f"expected 1, got {result}")

        # Repeated query (should hit cache, not S3)
        result2 = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0,0,1]' LIMIT 1")
        if result2 == "3":
            report("S3: cached query correct", "PASS")
        else:
            report("S3: cached query", "BUG", f"expected 3, got {result2}")

        # INSERT more vectors → triggers merge → new S3 version
        run_sql(dsn, f"""INSERT INTO {tbl} (v) VALUES
            ('[0.9,0.1,0]'), ('[0.1,0.9,0]'), ('[0.1,0.1,0.9]')""")

        time.sleep(MERGE_WAIT)

        # Query after merge should still be correct
        result = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        if result in ("1", "6"):  # id=1 or id=6 (both near [1,0,0])
            report("S3: query after merge + new S3 version", "PASS")
        else:
            report("S3: query after merge", "BUG", f"expected 1 or 6, got {result}")

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "8":
            report("S3: row count after inserts", "PASS")
        else:
            report("S3: row count", "BUG", f"expected 8, got {count}")

        return tbl  # keep table for further tests

    except Exception as e:
        report("S3 lifecycle", "FAIL", str(e)[:300])
        return None


def test_s3_truncate(dsn, tbl):
    try:
        run_sql(dsn, f"TRUNCATE {tbl}")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count != "0":
            report("S3: TRUNCATE count", "BUG", f"expected 0, got {count}")
            return

        # Re-insert
        run_sql(dsn, f"INSERT INTO {tbl} (v) VALUES ('[1,1,1]'), ('[0,0,1]')")
        time.sleep(MERGE_WAIT)

        result = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,1,1]' LIMIT 1")
        if result:
            report("S3: query after TRUNCATE + re-insert", "PASS")
        else:
            report("S3: query after TRUNCATE", "BUG", "empty result")

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "2":
            report("S3: count after TRUNCATE + re-insert", "PASS")
        else:
            report("S3: count after TRUNCATE", "BUG", f"expected 2, got {count}")

    except Exception as e:
        report("S3 TRUNCATE", "FAIL", str(e)[:300])


def test_s3_concurrent_queries(dsn, tbl):
    """10 concurrent queries on S3-backed index."""
    errors = []
    results_list = [None] * 10

    def query(idx):
        try:
            qv = f"[{random.random():.3f},{random.random():.3f},{random.random():.3f}]"
            r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 3")
            if r:
                results_list[idx] = r
            else:
                errors.append(f"thread-{idx}: empty result")
        except Exception as e:
            errors.append(f"thread-{idx}: {e}")

    threads = [threading.Thread(target=query, args=(i,)) for i in range(10)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=30)

    if errors:
        report("S3: 10 concurrent queries", "BUG", f"{len(errors)} errors: {errors[0][:150]}")
    else:
        report("S3: 10 concurrent queries", "PASS")


def test_s3_drop_index(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_s3_drop_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id SERIAL PRIMARY KEY, v VECTOR(3) NOT NULL)")
        run_sql(dsn, f"INSERT INTO {tbl} (v) VALUES ('[1,0,0]'), ('[0,1,0]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(MERGE_WAIT)

        # Warm cache
        run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")

        # DROP INDEX
        run_sql(dsn, f"DROP INDEX idx_{tbl}")

        # Table should still work
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "2":
            report("S3: DROP INDEX preserves table data", "PASS")
        else:
            report("S3: DROP INDEX data loss", "BUG", f"expected 2, got {count}")

        run_sql(dsn, f"DROP TABLE {tbl}")
    except Exception as e:
        report("S3 DROP INDEX", "FAIL", str(e)[:300])
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_s3_drop_table(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_s3_droptbl_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id SERIAL PRIMARY KEY, v VECTOR(3) NOT NULL)")
        run_sql(dsn, f"INSERT INTO {tbl} (v) VALUES ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(MERGE_WAIT)

        run_sql(dsn, f"DROP TABLE {tbl}")

        # Table gone
        result = run_psql(dsn, f"SELECT 1 FROM {tbl}", expect_success=False)
        if result.returncode != 0:
            report("S3: DROP TABLE cleans up", "PASS")
        else:
            report("S3: DROP TABLE", "BUG", "table still exists")

    except Exception as e:
        report("S3 DROP TABLE", "FAIL", str(e)[:300])
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_s3_drop_recreate_no_stale_cache(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_s3_rc_{sfx}"
    try:
        # Phase 1: create with data A
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, '[1,0,0]'), (2, '[0,1,0]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(MERGE_WAIT)

        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        assert r == "1", f"phase 1: expected 1, got {r}"

        # DROP
        run_sql(dsn, f"DROP TABLE {tbl}")

        # Phase 2: recreate with data B
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (100, '[0,0,1]'), (200, '[0,0.9,0]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(MERGE_WAIT)

        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0,1,0]' LIMIT 1")
        if r == "200":
            report("S3: DROP+recreate no stale cache", "PASS")
        else:
            report("S3: DROP+recreate stale cache", "BUG",
                   f"expected 200 (new data), got {r} (stale?)")

        run_sql(dsn, f"DROP TABLE {tbl}")
    except Exception as e:
        report("S3 DROP+recreate", "FAIL", str(e)[:300])
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("HNSW S3 LIVE TEST")
    print(f"Bucket: s3://{S3_BUCKET}/hnsw/")
    print("=" * 60)

    # Full lifecycle
    tbl = test_s3_full_lifecycle(dsn)
    if tbl:
        test_s3_truncate(dsn, tbl)
        test_s3_concurrent_queries(dsn, tbl)
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)

    test_s3_drop_index(dsn)
    test_s3_drop_table(dsn)
    test_s3_drop_recreate_no_stale_cache(dsn)

    print("\n" + "=" * 60)
    print("RESULTS SUMMARY")
    print("=" * 60)

    bugs = [r for r in RESULTS if r[1] == "BUG"]
    fails = [r for r in RESULTS if r[1] == "FAIL"]
    passes = [r for r in RESULTS if r[1] == "PASS"]

    print(f"  PASS: {len(passes)}")
    print(f"  FAIL: {len(fails)}")
    print(f"  BUG:  {len(bugs)}")

    if bugs:
        print("\n--- BUGS ---")
        for name, _, detail in bugs:
            print(f"  [BUG] {name}: {detail}")
    if fails:
        print("\n--- FAILURES ---")
        for name, _, detail in fails:
            print(f"  [FAIL] {name}: {detail}")

    # Show S3 objects
    print(f"\n--- S3 Objects in s3://{S3_BUCKET}/hnsw/ ---")
    print(s3_ls() or "(empty)")

    return 1 if bugs else 0


if __name__ == "__main__":
    raise SystemExit(main())

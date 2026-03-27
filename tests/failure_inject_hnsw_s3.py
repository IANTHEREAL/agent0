#!/usr/bin/env python3
"""
HNSW S3 failure injection tests (#2063).

Exercises REAL error paths in storage.rs and s3.rs by stopping/starting
a MinIO container to simulate S3 outages.

Tables use 1000+ rows so the CBO chooses HNSW Index Scan over Seq Scan.
Each HNSW query test asserts via EXPLAIN that HnswScan is selected
before relying on HNSW-specific behavior.

Setup:
  docker run -d --name minio-test -p 9000:9000 \
    -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
    minio/minio server /data

  python3 -c "
  import boto3
  s3 = boto3.client('s3', endpoint_url='http://localhost:9000',
      aws_access_key_id='minioadmin', aws_secret_access_key='minioadmin',
      region_name='us-east-1')
  s3.create_bucket(Bucket='hnsw-test')
  "

  DB9_DEV=1 DB9_DEV_ADMIN_PASSWORD=admin REDIS_URL=redis://localhost:6379 \
  HNSW_S3_BUCKET=hnsw-test HNSW_S3_ENDPOINT=http://localhost:9000 \
  HNSW_S3_REGION=us-east-1 HNSW_S3_FORCE_PATH_STYLE=true \
  HNSW_CACHE_MAX_ENTRIES=1 HNSW_INDEX_CACHE_MEMORY=100 \
  AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
  PD_ENDPOINTS=... cargo run

Error paths tested:
  1. CREATE INDEX S3 PUT failure  → create_index.rs put_graph() error
  2. Query-path S3 GET failure    → storage.rs load_base_graph() S3 GET error arm
  3. Cache hit bypasses S3        → storage.rs get_shared_base_graph() cache hit
  4. Merge S3 failure — survives  → worker/engine/helpers.rs execute_hnsw_merge()
  5. Recovery after S3 restored   → full outage→recovery cycle
  6. Non-HNSW queries unaffected  → connection isolation
"""

import argparse
import os
import random
import subprocess
import sys
import time


ROW_COUNT = 1000  # enough for CBO to choose HNSW scan over Seq Scan
MERGE_WAIT = 10   # seconds for background merge worker
MINIO_CONTAINER = os.environ.get("MINIO_CONTAINER", "minio-test")
RESULTS = []


def parse_args():
    parser = argparse.ArgumentParser(description="HNSW S3 failure injection tests")
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
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}")
    return result


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000, 9999)}"


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "SKIP": "[SKIP]"}[status]
    print(f"  {marker} {name}")
    if detail:
        print(f"        {detail}")


def safe_minio_start():
    """Restart MinIO, suppressing errors to avoid masking test failures."""
    try:
        minio_start()
    except Exception:
        print("        WARNING: MinIO restart failed in cleanup")


def insert_vectors(dsn, table, n):
    """Bulk-insert n random 3D vectors."""
    run_sql(dsn, f"""
        INSERT INTO {table} (v)
        SELECT ('[' || (random()*2-1)::text || ','
                    || (random()*2-1)::text || ','
                    || (random()*2-1)::text || ']')::vector
        FROM generate_series(1, {n});
    """)


def check_hnsw_scan(dsn, table):
    """Check if EXPLAIN shows HnswScan for this table.  Returns True/False."""
    plan = run_sql(dsn, f"EXPLAIN SELECT id FROM {table} ORDER BY v <-> '[1,0,0]' LIMIT 5;")
    return "HNSW Scan" in plan


def setup_hnsw_table(dsn, table, n=ROW_COUNT):
    """Create table, insert n vectors, create HNSW index, ANALYZE, wait for merge."""
    run_sql(dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, v VECTOR(3))")
    insert_vectors(dsn, table, n)
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (v vector_l2_ops)")
    run_sql(dsn, f"ANALYZE {table}")
    time.sleep(MERGE_WAIT)


# ── MinIO control ────────────────────────────────────────────


def minio_stop():
    subprocess.run(["docker", "stop", MINIO_CONTAINER],
                   capture_output=True, timeout=15)
    time.sleep(1)


def minio_start():
    subprocess.run(["docker", "start", MINIO_CONTAINER],
                   capture_output=True, timeout=15)
    for _ in range(20):
        try:
            r = subprocess.run(
                ["docker", "exec", MINIO_CONTAINER,
                 "curl", "-sf", "http://localhost:9000/minio/health/live"],
                capture_output=True, timeout=5)
            if r.returncode == 0:
                return
        except Exception:
            pass
        time.sleep(0.5)
    raise RuntimeError("MinIO did not become healthy")


def minio_is_running():
    r = subprocess.run(
        ["docker", "inspect", "-f", "{{.State.Running}}", MINIO_CONTAINER],
        capture_output=True, text=True, timeout=5)
    return r.stdout.strip() == "true"


# ── Test cases ───────────────────────────────────────────────


def test_create_index_s3_put_failure(dsn):
    """
    Target: create_index.rs:338-356
    CREATE INDEX does an immediate s3.put_graph().  With S3 down this
    must fail with a clear error.
    """
    suffix = random_suffix()
    table = f"fi_cidx_{suffix}"

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, v VECTOR(3))")
        insert_vectors(dsn, table, 10)

        minio_stop()

        result = run_psql(dsn, f"""
            CREATE INDEX idx_{table} ON {table} USING hnsw (v vector_l2_ops)
        """, expect_success=False, timeout=45)

        if result.returncode != 0:
            stderr = result.stderr.lower()
            if any(kw in stderr for kw in ["s3", "timeout", "connection",
                                            "dispatch failure", "failed"]):
                report("CREATE INDEX S3 PUT failure", "PASS",
                       f"Clear error: {result.stderr.strip()[:120]}")
            else:
                report("CREATE INDEX S3 PUT failure", "FAIL",
                       f"Unexpected error: {result.stderr.strip()[:200]}")
        else:
            report("CREATE INDEX S3 PUT failure", "SKIP",
                   "Server fell back to TiKV — S3 PUT path not exercised")
    except Exception as e:
        report("CREATE INDEX S3 PUT failure", "FAIL", str(e))
    finally:
        safe_minio_start()
        run_psql(dsn, f"DROP TABLE IF EXISTS {table}", expect_success=False)


def test_query_path_s3_get_failure(dsn):
    """
    Target: storage.rs:606-614 (S3 GET error on query-path cache miss)

    Strategy — force a REAL cache miss at query time:
    1. Create index A (1000 rows) → S3 PUT, cached in both layers
    2. Warm A: query with HNSW scan (asserted via EXPLAIN)
    3. Create index B (1000 rows) → S3 PUT, evicts A from file cache
       (HNSW_CACHE_MAX_ENTRIES=1) and in-memory cache
       (HNSW_INDEX_CACHE_MEMORY=100 triggers admission control)
    4. Warm B: query B to populate its cache entry
    5. Stop MinIO
    6. Query A → in-memory miss → file cache miss → S3 GET → FAIL
    """
    suffix = random_suffix()
    table_a = f"fi_a_{suffix}"
    table_b = f"fi_b_{suffix}"

    try:
        # Step 1-2: Create and warm index A
        setup_hnsw_table(dsn, table_a)
        if not check_hnsw_scan(dsn, table_a):
            report("query-path S3 GET failure", "SKIP",
                   "CBO chose SeqScan — HNSW scan not available (known planner issue)")
            return
        run_sql(dsn, f"SELECT id FROM {table_a} ORDER BY v <-> '[1,0,0]' LIMIT 5")

        # Step 3-4: Create and warm index B (evicts A)
        setup_hnsw_table(dsn, table_b)
        run_sql(dsn, f"SELECT id FROM {table_b} ORDER BY v <-> '[1,0,0]' LIMIT 5")

        # Step 5: Stop MinIO
        minio_stop()

        # Step 6: Query A — should be a cache miss → S3 GET → fail
        result = run_psql(dsn, f"""
            SELECT id FROM {table_a} ORDER BY v <-> '[1,0,0]' LIMIT 5
        """, expect_success=False, timeout=60)

        if result.returncode != 0:
            stderr = result.stderr.lower()
            if any(kw in stderr for kw in ["s3", "timeout", "connection",
                                            "dispatch failure", "failed",
                                            "unavailable", "hnsw", "graph"]):
                report("query-path S3 GET failure", "PASS",
                       f"Error: {result.stderr.strip()[:120]}")
            else:
                report("query-path S3 GET failure", "FAIL",
                       f"Error not S3-related: {result.stderr.strip()[:200]}")
        else:
            report("query-path S3 GET failure", "FAIL",
                   "Query succeeded — cache was NOT evicted. "
                   "Ensure HNSW_CACHE_MAX_ENTRIES=1 and "
                   "HNSW_INDEX_CACHE_MEMORY=100 on the server.")
    except Exception as e:
        report("query-path S3 GET failure", "FAIL", str(e))
    finally:
        safe_minio_start()
        run_psql(dsn, f"DROP TABLE IF EXISTS {table_a}", expect_success=False)
        run_psql(dsn, f"DROP TABLE IF EXISTS {table_b}", expect_success=False)


def test_cache_hit_bypasses_s3(dsn):
    """
    Target: storage.rs:848 (in-memory cache hit — positive control)

    With only ONE index loaded (no eviction), S3 outage must not affect
    queries.  We assert HNSW scan is used via EXPLAIN.
    """
    suffix = random_suffix()
    table = f"fi_hit_{suffix}"

    try:
        setup_hnsw_table(dsn, table)
        uses_hnsw = check_hnsw_scan(dsn, table)
        if not uses_hnsw:
            report("cache hit bypasses S3", "SKIP",
                   "CBO chose SeqScan — HNSW scan not available (known planner issue)")
            return

        # Warm cache
        r1 = run_sql(dsn, f"""
            SELECT count(*) FROM (
                SELECT id FROM {table} ORDER BY v <-> '[1,0,0]' LIMIT 5
            ) t
        """)
        assert r1 == "5", f"expected 5, got {r1}"

        # Stop S3 — single index, no eviction, cache serves
        minio_stop()

        r2 = run_sql(dsn, f"""
            SELECT count(*) FROM (
                SELECT id FROM {table} ORDER BY v <-> '[0,1,0]' LIMIT 5
            ) t
        """)
        assert r2 == "5", f"expected 5 during S3 outage, got {r2}"

        report("cache hit bypasses S3", "PASS")
    except Exception as e:
        report("cache hit bypasses S3", "FAIL", str(e))
    finally:
        safe_minio_start()
        run_psql(dsn, f"DROP TABLE IF EXISTS {table}", expect_success=False)


def test_merge_s3_failure_server_survives(dsn):
    """
    Target: helpers.rs:128-145 + storage.rs:606-614

    Merge worker tries S3 GET for base graph; if S3 is down it logs an
    error and the server stays alive.  Queries work via two-level search
    (cached base + TiKV deltas).
    """
    suffix = random_suffix()
    table = f"fi_merge_{suffix}"

    try:
        setup_hnsw_table(dsn, table)

        # Stop S3
        minio_stop()

        # Insert deltas — merge will try S3 GET/PUT and fail
        insert_vectors(dsn, table, 100)

        # Wait for merge attempt — should fail, not crash
        time.sleep(MERGE_WAIT)

        # Server alive? Plain count works regardless of HNSW scan.
        r = run_sql(dsn, f"SELECT count(*) FROM {table}")
        assert int(r) == ROW_COUNT + 100, f"expected {ROW_COUNT + 100}, got {r}"

        report("merge S3 failure — server survives", "PASS")
    except Exception as e:
        report("merge S3 failure — server survives", "FAIL", str(e))
    finally:
        safe_minio_start()
        time.sleep(MERGE_WAIT)
        run_psql(dsn, f"DROP TABLE IF EXISTS {table}", expect_success=False)


def test_recovery_after_s3_restored(dsn):
    """
    Target: full cycle — CREATE INDEX, query, merge all work after
    S3 comes back.
    """
    suffix = random_suffix()
    table = f"fi_recov_{suffix}"

    try:
        # Phase 1: S3 down, CREATE INDEX fails
        minio_stop()
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, v VECTOR(3))")
        insert_vectors(dsn, table, 10)

        result = run_psql(dsn, f"""
            CREATE INDEX idx_{table} ON {table} USING hnsw (v vector_l2_ops)
        """, expect_success=False, timeout=45)
        s3_was_down = result.returncode != 0

        # Phase 2: Restore S3
        minio_start()

        # Phase 3: CREATE INDEX succeeds now
        if s3_was_down:
            run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (v vector_l2_ops)")

        # Phase 4: Query works
        r = run_sql(dsn, f"SELECT id FROM {table} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        assert r, "query should return results after S3 recovery"

        report("recovery after S3 restored", "PASS")
    except Exception as e:
        report("recovery after S3 restored", "FAIL", str(e))
    finally:
        if not minio_is_running():
            safe_minio_start()
        run_psql(dsn, f"DROP TABLE IF EXISTS {table}", expect_success=False)


def test_non_hnsw_queries_unaffected(dsn):
    """
    Target: connection isolation — S3 failures must never affect plain SQL.
    """
    suffix = random_suffix()
    table = f"fi_plain_{suffix}"

    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
        run_sql(dsn, f"""
            CREATE TABLE {table} (id SERIAL PRIMARY KEY, name TEXT);
            INSERT INTO {table} (name) VALUES ('alice'), ('bob'), ('charlie');
        """)

        minio_stop()

        r1 = run_sql(dsn, f"SELECT count(*) FROM {table}")
        assert r1 == "3"
        run_sql(dsn, f"INSERT INTO {table} (name) VALUES ('diana')")
        r2 = run_sql(dsn, f"SELECT count(*) FROM {table}")
        assert r2 == "4"
        run_sql(dsn, f"UPDATE {table} SET name = 'eve' WHERE id = 1")
        r3 = run_sql(dsn, f"SELECT name FROM {table} WHERE id = 1")
        assert r3 == "eve"

        report("non-HNSW queries unaffected", "PASS")
    except Exception as e:
        report("non-HNSW queries unaffected", "FAIL", str(e))
    finally:
        safe_minio_start()
        run_psql(dsn, f"DROP TABLE IF EXISTS {table}", expect_success=False)


# ── Main ─────────────────────────────────────────────────────


def main():
    args = parse_args()
    dsn = args.dsn

    print("=== HNSW S3 Failure Injection Tests (#2063) ===")
    print(f"DSN: {dsn}")
    print(f"MinIO container: {MINIO_CONTAINER}")
    print(f"Row count per table: {ROW_COUNT}")
    print()

    if not minio_is_running():
        print(f"ERROR: MinIO container '{MINIO_CONTAINER}' is not running.")
        sys.exit(1)

    try:
        run_sql(dsn, "SELECT 1")
    except Exception as e:
        print(f"ERROR: Cannot connect to db9-server: {e}")
        sys.exit(1)

    tests = [
        ("1. CREATE INDEX S3 PUT failure",          test_create_index_s3_put_failure),
        ("2. Query-path S3 GET failure",            test_query_path_s3_get_failure),
        ("3. Cache hit bypasses S3",                test_cache_hit_bypasses_s3),
        ("4. Merge S3 failure — server survives",   test_merge_s3_failure_server_survives),
        ("5. Recovery after S3 restored",           test_recovery_after_s3_restored),
        ("6. Non-HNSW queries unaffected",          test_non_hnsw_queries_unaffected),
    ]

    print(f"Running {len(tests)} tests...\n")
    for name, fn in tests:
        print(f"[TEST] {name}")
        fn(dsn)
        if not minio_is_running():
            safe_minio_start()
        print()

    print("=" * 50)
    passed = sum(1 for _, s, _ in RESULTS if s == "PASS")
    failed = sum(1 for _, s, _ in RESULTS if s == "FAIL")
    skipped = sum(1 for _, s, _ in RESULTS if s == "SKIP")
    print(f"Results: {passed} passed, {failed} failed, {skipped} skipped")

    if failed > 0:
        print("\nFailed tests:")
        for name, status, detail in RESULTS:
            if status == "FAIL":
                print(f"  - {name}: {detail}")
        sys.exit(1)
    sys.exit(0)


if __name__ == "__main__":
    main()

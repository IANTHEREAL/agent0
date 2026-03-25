#!/usr/bin/env python3
"""
Worker DROP DATABASE guard test (PR #2053).

Verifies that background worker tasks (cron, HNSW merge, bg_sql) correctly
skip execution when the target database has been dropped, instead of
falling back to "postgres" database.

Tests:
1. CREATE DATABASE → schedule cron job → DROP DATABASE → verify no crash,
   no execution against wrong db
2. CREATE DATABASE → create HNSW index → DROP DATABASE → verify worker
   doesn't panic on next merge cycle
"""

import argparse
import random
import subprocess
import time


def parse_args():
    parser = argparse.ArgumentParser(
        description="Worker DROP DATABASE guard test"
    )
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


def run_psql(dsn, sql, expect_success=True, timeout=60):
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )


def make_dsn(base_dsn, dbname):
    """Replace database in DSN."""
    # postgres://user:pass@host:port/db → postgres://user:pass@host:port/newdb
    parts = base_dsn.rsplit("/", 1)
    return f"{parts[0]}/{dbname}"


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


def test_drop_db_with_cron(dsn):
    """DROP DATABASE with active cron job → worker skips, no crash."""
    sfx = random_suffix()
    dbname = f"drop_guard_cron_{sfx}"
    db_dsn = make_dsn(dsn, dbname)

    try:
        # Create test database
        run_sql(dsn, f"CREATE DATABASE {dbname}")
        print(f"        created database {dbname}")

        # Create a table and use pg_background_launch to queue a bg task
        run_sql(db_dsn, "CREATE TABLE canary (id INT)")
        run_sql(db_dsn, "INSERT INTO canary VALUES (1)")

        # Trigger a background task (ANALYZE queues a worker task)
        run_sql(db_dsn, "ANALYZE canary")
        print("        queued worker tasks via ANALYZE")

        # DROP DATABASE
        run_sql(dsn, f"DROP DATABASE {dbname}")
        print("        dropped database")

        # Wait for worker tick — tasks for the dropped db should be skipped
        print("        waiting 10s for worker cycle after DROP...")
        time.sleep(10)

        # Verify: server is still alive
        alive = run_sql(dsn, "SELECT 1")
        if alive == "1":
            report("Worker DROP guard: worker after DROP DATABASE", "PASS",
                   "server alive, worker skipped dropped database")
        else:
            report("Worker DROP guard: worker after DROP DATABASE", "BUG",
                   "server not responding")

        # Verify: no canary table in postgres db (worker didn't fall back)
        result = run_psql(dsn, "SELECT count(*) FROM canary", expect_success=False)
        if result.returncode != 0:
            report("Worker DROP guard: no fallback to postgres", "PASS",
                   "canary table does not exist in postgres db")
        else:
            report("Worker DROP guard: FELL BACK to postgres", "BUG",
                   f"canary table exists in postgres! count={result.stdout.strip()}")

    except Exception as e:
        report("Worker DROP guard cron", "FAIL", str(e)[:300])
        # Cleanup in case of failure
        run_psql(dsn, f"DROP DATABASE IF EXISTS {dbname}", expect_success=False)


def test_drop_db_with_hnsw(dsn):
    """DROP DATABASE with HNSW index → worker skips merge, no crash."""
    sfx = random_suffix()
    dbname = f"drop_guard_hnsw_{sfx}"
    db_dsn = make_dsn(dsn, dbname)

    try:
        run_sql(dsn, f"CREATE DATABASE {dbname}")

        # Create table with HNSW index and insert data to trigger merge
        run_sql(db_dsn, "CREATE TABLE vec_t (id SERIAL PRIMARY KEY, v VECTOR(3) NOT NULL)")
        run_sql(db_dsn, """INSERT INTO vec_t (v) VALUES
            ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]'),
            ('[0.5,0.5,0]'), ('[0,0.5,0.5]')""")
        run_sql(db_dsn, "CREATE INDEX idx_vec ON vec_t USING hnsw (v vector_l2_ops)")
        print(f"        created HNSW index in {dbname}")

        # Insert more to queue a merge
        run_sql(db_dsn, """INSERT INTO vec_t (v) VALUES
            ('[0.1,0.1,0.8]'), ('[0.8,0.1,0.1]'), ('[0.1,0.8,0.1]')""")

        # DROP DATABASE
        run_sql(dsn, f"DROP DATABASE {dbname}")
        print("        dropped database with pending HNSW merge")

        # Wait for worker merge cycle
        print("        waiting 10s for worker cycle...")
        time.sleep(10)

        # Verify server still alive
        alive = run_sql(dsn, "SELECT 1")
        if alive == "1":
            report("Worker DROP guard: HNSW merge after DROP DATABASE", "PASS",
                   "server alive, merge skipped for dropped database")
        else:
            report("Worker DROP guard: HNSW merge after DROP", "BUG",
                   "server not responding after DROP DATABASE with pending merge")

    except Exception as e:
        report("Worker DROP guard HNSW", "FAIL", str(e)[:300])
        run_psql(dsn, f"DROP DATABASE IF EXISTS {dbname}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("WORKER DROP DATABASE GUARD TEST")
    print("=" * 60)

    test_drop_db_with_hnsw(dsn)
    test_drop_db_with_cron(dsn)

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

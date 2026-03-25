#!/usr/bin/env python3
"""
Safety regression tests for HNSW S3 graph offload (PRs #1970, #2020).

Two modes:
  1. TiKV-only mode (no HNSW_S3_BUCKET): always runs, validates no regression.
  2. S3 mode (HNSW_S3_BUCKET set): tests full S3 lifecycle.

S3 tests are SKIPPED (exit 0) when HNSW_S3_BUCKET is not set on the server.
To run S3 tests, start db9-server with:
  HNSW_S3_BUCKET=hnsw-test HNSW_S3_ENDPOINT=http://localhost:9000 \
  HNSW_S3_REGION=us-east-1 HNSW_S3_FORCE_PATH_STYLE=true \
  AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
  PD_ENDPOINTS=... cargo run
"""

import argparse
import os
import random
import string
import subprocess
import sys
import time


MERGE_WAIT_SEC = 5  # Wait for background merge to run


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="HNSW S3 offload safety regression tests"
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


def detect_s3_mode(dsn: str) -> bool:
    """Check if the server has S3 configured by attempting to create an S3-mode index.
    We use a GUC or log-based detection. Simplest: check if env var is set locally
    (the test runner is assumed to know the server config)."""
    # If the caller set HNSW_S3_TEST=1 explicitly, trust that.
    return os.environ.get("HNSW_S3_TEST", "0") == "1"


# ===========================================================================
# TiKV-only tests (always run)
# ===========================================================================

def test_tikv_create_insert_query(dsn: str, suffix: str) -> None:
    """CREATE INDEX → INSERT → query lifecycle on TiKV-only path."""
    table = f"hnsw_tikv_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]')")

    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,0,0]' LIMIT 1")
    assert result == "1", f"Expected id=1 as nearest neighbor, got: {result}"

    # Verify count
    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "3", f"Expected 3 rows, got: {count}"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] TiKV-1: CREATE INDEX + INSERT + query lifecycle")


def test_tikv_drop_index(dsn: str, suffix: str) -> None:
    """DROP INDEX should leave table data intact."""
    table = f"hnsw_drop_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]')")

    time.sleep(MERGE_WAIT_SEC)

    run_sql(dsn, f"DROP INDEX idx_{table}")
    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "2", f"Expected 2 rows after DROP INDEX, got: {count}"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] TiKV-2: DROP INDEX preserves table data")


def test_tikv_truncate(dsn: str, suffix: str) -> None:
    """TRUNCATE with HNSW index should clear data and allow re-insert."""
    table = f"hnsw_trunc_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]')")

    time.sleep(MERGE_WAIT_SEC)

    run_sql(dsn, f"TRUNCATE {table}")
    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "0", f"Expected 0 rows after TRUNCATE, got: {count}"

    # Re-insert after truncate
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,1,1]')")
    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,1,1]' LIMIT 1")
    assert result != "", f"Expected a result after post-TRUNCATE insert, got empty"

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "1", f"Expected 1 row after re-insert, got: {count}"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] TiKV-3: TRUNCATE + re-insert lifecycle")


def test_tikv_drop_table(dsn: str, suffix: str) -> None:
    """DROP TABLE with HNSW index should cleanly remove everything."""
    table = f"hnsw_droptbl_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]')")

    time.sleep(MERGE_WAIT_SEC)

    run_sql(dsn, f"DROP TABLE {table}")

    # Verify table no longer exists
    result = run_psql(dsn, f"SELECT 1 FROM {table}", expect_success=False)
    assert result.returncode != 0, "Table should not exist after DROP TABLE"

    print("  [OK] TiKV-4: DROP TABLE with HNSW index")


def test_tikv_multiple_distance_metrics(dsn: str, suffix: str) -> None:
    """Verify L2, cosine, and inner product distance metrics."""
    table = f"hnsw_metrics_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]')")

    # L2 distance
    run_sql(dsn, f"CREATE INDEX idx_{table}_l2 ON {table} USING hnsw (embedding vector_l2_ops)")
    time.sleep(MERGE_WAIT_SEC)
    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,0,0]' LIMIT 1")
    assert result == "1", f"L2: expected id=1, got {result}"
    run_sql(dsn, f"DROP INDEX idx_{table}_l2")

    # Cosine distance
    run_sql(dsn, f"CREATE INDEX idx_{table}_cos ON {table} USING hnsw (embedding vector_cosine_ops)")
    time.sleep(MERGE_WAIT_SEC)
    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <=> '[1,0,0]' LIMIT 1")
    assert result == "1", f"Cosine: expected id=1, got {result}"
    run_sql(dsn, f"DROP INDEX idx_{table}_cos")

    # Inner product
    run_sql(dsn, f"CREATE INDEX idx_{table}_ip ON {table} USING hnsw (embedding vector_ip_ops)")
    time.sleep(MERGE_WAIT_SEC)
    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <#> '[1,0,0]' LIMIT 1")
    assert result == "1", f"IP: expected id=1, got {result}"
    run_sql(dsn, f"DROP INDEX idx_{table}_ip")

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] TiKV-5: Multiple distance metrics (L2, cosine, inner product)")


def test_tikv_drop_recreate_index(dsn: str, suffix: str) -> None:
    """DROP INDEX + CREATE INDEX with same name (no stale cache)."""
    table = f"hnsw_recreate_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]')")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")

    time.sleep(MERGE_WAIT_SEC)

    # Query to warm any cache
    run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,0,0]' LIMIT 1")

    # Drop and change data
    run_sql(dsn, f"DROP INDEX idx_{table}")
    run_sql(dsn, f"DELETE FROM {table}")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[0,0,1]')")

    # Recreate index
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")
    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[0,0,1]' LIMIT 1")
    assert result != "", f"Expected result after index recreate, got empty"

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "1", f"Expected 1 row after recreate, got {count}"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] TiKV-6: DROP + recreate index (no stale cache)")


# ===========================================================================
# S3-mode tests (only when HNSW_S3_TEST=1)
# ===========================================================================

def test_s3_full_lifecycle(dsn: str, suffix: str) -> None:
    """S3 mode: CREATE INDEX → INSERT → merge → query → more inserts → query."""
    table = f"hnsw_s3_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")

    # Initial data + index
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]')")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")

    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,0,0]' LIMIT 1")
    assert result == "1", f"S3: expected id=1, got {result}"

    # Add more vectors to trigger another merge
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[0.5,0.5,0]'), ('[0,0.5,0.5]')")
    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[0.5,0.5,0]' LIMIT 1")
    assert result == "4", f"S3 post-merge: expected id=4, got {result}"

    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "5", f"S3: expected 5 rows, got {count}"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] S3-1: Full lifecycle (create + insert + merge + query)")


def test_s3_truncate(dsn: str, suffix: str) -> None:
    """S3 mode: TRUNCATE with HNSW index."""
    table = f"hnsw_s3_trunc_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]')")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")

    time.sleep(MERGE_WAIT_SEC)

    run_sql(dsn, f"TRUNCATE {table}")
    count = run_sql(dsn, f"SELECT count(*) FROM {table}")
    assert count == "0", f"S3 TRUNCATE: expected 0 rows, got {count}"

    # Re-insert and query
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,1,1]')")
    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,1,1]' LIMIT 1")
    assert result != "", f"S3 TRUNCATE: expected result after re-insert"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] S3-2: TRUNCATE + re-insert lifecycle")


def test_s3_drop_recreate(dsn: str, suffix: str) -> None:
    """S3 mode: DROP INDEX + CREATE INDEX (cache invalidation)."""
    table = f"hnsw_s3_recreate_{suffix}"
    run_sql(dsn, f"DROP TABLE IF EXISTS {table}")
    run_sql(dsn, f"CREATE TABLE {table} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[1,0,0]')")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")

    time.sleep(MERGE_WAIT_SEC)

    # Warm cache
    run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[1,0,0]' LIMIT 1")

    # Drop, change data, recreate
    run_sql(dsn, f"DROP INDEX idx_{table}")
    run_sql(dsn, f"DELETE FROM {table}")
    run_sql(dsn, f"INSERT INTO {table} (embedding) VALUES ('[0,0,1]')")
    run_sql(dsn, f"CREATE INDEX idx_{table} ON {table} USING hnsw (embedding vector_l2_ops)")

    time.sleep(MERGE_WAIT_SEC)

    result = run_sql(dsn, f"SELECT id FROM {table} ORDER BY embedding <-> '[0,0,1]' LIMIT 1")
    assert result != "", f"S3 recreate: expected result after index recreate"

    run_sql(dsn, f"DROP TABLE {table}")
    print("  [OK] S3-3: DROP + recreate index (S3 cache invalidation)")


def main() -> int:
    args = parse_args()
    dsn = args.dsn
    suffix = random_suffix()
    s3_mode = detect_s3_mode(dsn)

    print(f"[INFO] suffix={suffix}, s3_mode={s3_mode}")

    try:
        # TiKV-only tests (always run)
        print("[PHASE 1] TiKV-only tests")
        test_tikv_create_insert_query(dsn, suffix)
        test_tikv_drop_index(dsn, suffix)
        test_tikv_truncate(dsn, suffix)
        test_tikv_drop_table(dsn, suffix)
        test_tikv_multiple_distance_metrics(dsn, suffix)
        test_tikv_drop_recreate_index(dsn, suffix)

        tikv_count = 6

        if s3_mode:
            # S3 tests (only when HNSW_S3_TEST=1)
            print("[PHASE 2] S3 offload tests")
            test_s3_full_lifecycle(dsn, suffix)
            test_s3_truncate(dsn, suffix)
            test_s3_drop_recreate(dsn, suffix)
            s3_count = 3
            print(f"PASS: HNSW S3 offload safety tests ({tikv_count + s3_count}/{tikv_count + s3_count})")
        else:
            print("[SKIP] S3 tests skipped (set HNSW_S3_TEST=1 to enable)")
            print(f"PASS: HNSW TiKV-only safety tests ({tikv_count}/{tikv_count})")

        return 0
    except AssertionError as exc:
        print(f"FAIL: assertion failed: {exc}")
        return 1
    except Exception as exc:
        print(f"FAIL: runtime error: {exc}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

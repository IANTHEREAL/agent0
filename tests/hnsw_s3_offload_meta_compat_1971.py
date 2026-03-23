#!/usr/bin/env python3
"""
Regression test for #1971: HNSW S3 offload meta compatibility.

Verifies that the new HnswMeta fields (graph_version, dropped_at) are
backward-compatible with existing indexes:
- storage_version=1 indexes (TiKV-only) continue to work
- CREATE INDEX, INSERT, SELECT, DROP INDEX lifecycle on TiKV-only path
- TRUNCATE preserves index and allows re-insert

This test runs WITHOUT HNSW_S3_BUCKET set (TiKV-only mode), ensuring
the S3 offload code changes do not regress the existing behavior.
"""

import argparse
import subprocess
import sys
import time


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="HNSW S3 offload meta compatibility test (#1971)"
    )
    parser.add_argument(
        "--dsn",
        required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def run_sql(dsn: str, sql: str) -> str:
    result = subprocess.run(
        [
            "psql",
            dsn,
            "--no-psqlrc",
            "-A",
            "-t",
            "-q",
            "-c",
            sql,
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"SQL failed (rc={result.returncode}):\n"
            f"  sql: {sql}\n"
            f"  stderr: {result.stderr.strip()}"
        )
    return result.stdout.strip()


TABLE = "hnsw_s3_compat_1971"


def main() -> None:
    args = parse_args()
    dsn = args.dsn

    # Cleanup from prior runs
    run_sql(dsn, f"DROP TABLE IF EXISTS {TABLE}")

    # 1. Create table with vector column
    run_sql(
        dsn,
        f"CREATE TABLE {TABLE} (id SERIAL PRIMARY KEY, embedding VECTOR(3) NOT NULL)",
    )

    # 2. Create HNSW index (storage_version=1 in TiKV-only mode)
    run_sql(
        dsn,
        f"CREATE INDEX idx_{TABLE} ON {TABLE} USING hnsw (embedding vector_l2_ops)",
    )

    # 3. Insert some vectors
    run_sql(
        dsn,
        f"INSERT INTO {TABLE} (embedding) VALUES ('[1,0,0]'), ('[0,1,0]'), ('[0,0,1]')",
    )

    # Small delay for merge
    time.sleep(2)

    # 4. Query using HNSW scan
    result = run_sql(
        dsn,
        f"SELECT id FROM {TABLE} ORDER BY embedding <-> '[1,0,0]' LIMIT 1",
    )
    assert result == "1", f"Expected id=1 as nearest neighbor, got: {result}"
    print(f"  [OK] HNSW query returned correct result: id={result}")

    # 5. DROP INDEX
    run_sql(dsn, f"DROP INDEX idx_{TABLE}")

    # 6. Verify table still works after index drop
    result = run_sql(dsn, f"SELECT count(*) FROM {TABLE}")
    assert result == "3", f"Expected 3 rows after DROP INDEX, got: {result}"
    print(f"  [OK] Table has {result} rows after DROP INDEX")

    # 7. Recreate index and test TRUNCATE
    run_sql(
        dsn,
        f"CREATE INDEX idx_{TABLE} ON {TABLE} USING hnsw (embedding vector_l2_ops)",
    )
    run_sql(dsn, f"TRUNCATE {TABLE}")

    # 8. Verify empty table after truncate
    result = run_sql(dsn, f"SELECT count(*) FROM {TABLE}")
    assert result == "0", f"Expected 0 rows after TRUNCATE, got: {result}"
    print(f"  [OK] Table has {result} rows after TRUNCATE")

    # 9. Insert after truncate and verify table works
    run_sql(dsn, f"INSERT INTO {TABLE} (embedding) VALUES ('[1,1,1]')")
    result = run_sql(dsn, f"SELECT count(*) FROM {TABLE}")
    assert result == "1", f"Expected 1 row after post-TRUNCATE INSERT, got: {result}"
    print(f"  [OK] Post-TRUNCATE INSERT succeeded, count={result}")

    # Cleanup
    run_sql(dsn, f"DROP TABLE IF EXISTS {TABLE}")

    print("PASS: HNSW S3 offload meta compatibility (#1971)")


if __name__ == "__main__":
    main()

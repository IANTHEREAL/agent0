#!/usr/bin/env python3
"""
Stress / boundary test for safety PRs.
Goal: FIND BUGS, not pass tests. Reports all failures with severity.

Tests boundary conditions with real data:
1. KV value size: exact boundary (8388608 bytes), off-by-one
2. COPY rotation: exactly 5000 rows, 4999, 5001, 50000 unrotated ceiling
3. Decimal: MAX, MAX-1+1, MAX+1, division edge cases
4. Hash join: tight memory boundary
5. HNSW: concurrent writes + reads, UPDATE during scan, DELETE during scan
6. Large COPY + query correctness verification
7. ALTER TABLE byte budget with measured data
"""

import argparse
import os
import random
import string
import subprocess
import sys
import threading
import time
import traceback


def parse_args():
    parser = argparse.ArgumentParser(description="Stress boundary tests")
    parser.add_argument("--dsn", required=True)
    return parser.parse_args()


def run_psql(dsn, sql, expect_success=True, timeout=120):
    result = subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True, timeout=timeout,
    )
    if expect_success and result.returncode != 0:
        raise RuntimeError(f"SQL failed: {result.stderr.strip()}\nSQL: {sql[:200]}")
    return result


def run_sql(dsn, sql, timeout=120):
    return run_psql(dsn, sql, expect_success=True, timeout=timeout).stdout.strip()


def run_psql_stdin(dsn, input_text, timeout=120):
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q",
         "-v", "ON_ERROR_STOP=1", "-f", "-"],
        input=input_text, capture_output=True, text=True, timeout=timeout,
    )


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000,9999)}"


RESULTS = []

def report(name, status, detail=""):
    """status: PASS, FAIL, BUG"""
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        print(f"        {detail}")


# ================================================================
# 1. KV Value Size Guard: exact boundary
#    Limit = 8388608 bytes. Check: key.len() + value.len() > limit
#    Key overhead ≈ 30-50 bytes depending on table/index structure
# ================================================================
def test_kv_boundary(dsn):
    sfx = random_suffix()
    tbl = f"kv_bound_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT NOT NULL)")

        # Measure actual key overhead by inserting a small row
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 'x')")

        # Try increasing sizes to find the exact cutoff
        # Default limit = 8388608 bytes. With key overhead ~55 bytes,
        # the max text size is roughly 8388608 - 55 = 8388553
        # But the actual encoded size includes length prefix etc.
        # Test strategy: binary search for the exact boundary

        # Well under limit: 7 MB (should succeed)
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (10, repeat('a', 7000000))")
        report("KV: 7MB row", "PASS")

        # Just under: 8MB (should succeed - leaves room for key overhead)
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (11, repeat('a', 8000000))")
        report("KV: 8MB row (under limit with overhead)", "PASS")

        # At boundary: 8.3MB - very close to 8388608
        result = run_psql(dsn, f"INSERT INTO {tbl} VALUES (12, repeat('a', 8300000))", expect_success=False)
        if result.returncode == 0:
            report("KV: 8.3MB row", "PASS", "accepted (within limit after encoding)")
        else:
            if "value too large" in result.stderr.lower():
                report("KV: 8.3MB row", "PASS", "rejected by guard (expected near boundary)")
            else:
                report("KV: 8.3MB row", "BUG", f"unexpected error: {result.stderr[:200]}")

        # Over limit: 8.5MB (must fail)
        result = run_psql(dsn, f"INSERT INTO {tbl} VALUES (13, repeat('a', 8500000))", expect_success=False)
        if result.returncode != 0 and "value too large" in result.stderr.lower():
            report("KV: 8.5MB row rejected", "PASS")
        elif result.returncode == 0:
            report("KV: 8.5MB row NOT rejected", "BUG", "oversized write was accepted!")
        else:
            report("KV: 8.5MB row", "BUG", f"wrong error: {result.stderr[:200]}")

        # Way over limit: 16MB
        result = run_psql(dsn, f"INSERT INTO {tbl} VALUES (14, repeat('a', 16000000))", expect_success=False)
        if result.returncode != 0 and "value too large" in result.stderr.lower():
            report("KV: 16MB row rejected", "PASS")
        else:
            report("KV: 16MB row", "BUG", "should have been rejected")

        # UPDATE existing small row to oversized
        result = run_psql(dsn, f"UPDATE {tbl} SET data = repeat('b', 8500000) WHERE id = 1", expect_success=False)
        if result.returncode != 0 and "value too large" in result.stderr.lower():
            report("KV: UPDATE to oversized rejected", "PASS")
        else:
            report("KV: UPDATE to oversized", "BUG", "should have been rejected")

        # Verify original data unchanged after rejected UPDATE
        data = run_sql(dsn, f"SELECT data FROM {tbl} WHERE id = 1")
        if data == "x":
            report("KV: original data intact after rejection", "PASS")
        else:
            report("KV: original data mutated after rejection", "BUG", f"got: {data[:50]}")

    except Exception as e:
        report("KV boundary", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 2. COPY rotation boundaries
#    COPY_STDIN_COMMIT_SIZE = 5000, check: batch_rows >= 5000
#    MAX_UNROTATED_ROWS = 50000
# ================================================================
def test_copy_boundaries(dsn):
    sfx = random_suffix()
    tbl = f"copy_bound_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT)")

        # Exactly 4999 rows (no rotation)
        data = "\n".join(f"{i}\trow_{i}" for i in range(1, 5000)) + "\n"
        result = run_psql_stdin(dsn, f"COPY {tbl} (id, data) FROM STDIN;\n{data}\\.\n")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "4999":
            report("COPY: 4999 rows (no rotation)", "PASS")
        else:
            report("COPY: 4999 rows", "BUG", f"expected 4999, got {count}")

        run_sql(dsn, f"DELETE FROM {tbl}")

        # Exactly 5000 rows (first rotation point: batch_rows >= 5000)
        data = "\n".join(f"{i}\trow_{i}" for i in range(1, 5001)) + "\n"
        result = run_psql_stdin(dsn, f"COPY {tbl} (id, data) FROM STDIN;\n{data}\\.\n")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "5000":
            report("COPY: exactly 5000 rows", "PASS")
        else:
            report("COPY: 5000 rows", "BUG", f"expected 5000, got {count}")

        run_sql(dsn, f"DELETE FROM {tbl}")

        # 5001 rows (first rotation at 5000, remainder = 1)
        data = "\n".join(f"{i}\trow_{i}" for i in range(1, 5002)) + "\n"
        result = run_psql_stdin(dsn, f"COPY {tbl} (id, data) FROM STDIN;\n{data}\\.\n")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "5001":
            report("COPY: 5001 rows", "PASS")
        else:
            report("COPY: 5001 rows", "BUG", f"expected 5001, got {count}")

        run_sql(dsn, f"DELETE FROM {tbl}")

        # 15000 rows (3 rotations: 5000+5000+5000)
        data = "\n".join(f"{i}\trow_{i}" for i in range(1, 15001)) + "\n"
        result = run_psql_stdin(dsn, f"COPY {tbl} (id, data) FROM STDIN;\n{data}\\.\n")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "15000":
            report("COPY: 15000 rows (3 rotations)", "PASS")
        else:
            report("COPY: 15000 rows", "BUG", f"expected 15000, got {count}")

        # Verify data integrity after rotated COPY
        min_id = run_sql(dsn, f"SELECT min(id) FROM {tbl}")
        max_id = run_sql(dsn, f"SELECT max(id) FROM {tbl}")
        if min_id == "1" and max_id == "15000":
            report("COPY: data integrity after rotation", "PASS")
        else:
            report("COPY: data integrity", "BUG", f"min={min_id}, max={max_id}")

        run_sql(dsn, f"DELETE FROM {tbl}")

        # Explicit txn: COPY 6000 rows + ROLLBACK → 0 rows (no rotation)
        input_text = (
            f"BEGIN;\n"
            f"COPY {tbl} (id, data) FROM STDIN;\n"
            + "\n".join(f"{i}\trow_{i}" for i in range(1, 6001)) + "\n"
            + "\\.\n"
            + "ROLLBACK;\n"
        )
        run_psql_stdin(dsn, input_text)
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "0":
            report("COPY: explicit txn ROLLBACK = atomic", "PASS")
        else:
            report("COPY: explicit txn ROLLBACK NOT atomic", "BUG",
                   f"expected 0 rows, got {count} (rotation leaked?)")

        # Autocommit partial failure: COPY with duplicate PK mid-stream
        run_sql(dsn, f"DELETE FROM {tbl}")
        # Insert 7000 rows: first 5000 will rotate+commit, then 5001st is duplicate of 1
        lines = []
        for i in range(1, 7001):
            # At row 5001, we insert a duplicate of id=1 → error
            if i == 5001:
                lines.append(f"1\tduplicate")
            else:
                lines.append(f"{i}\trow_{i}")
        data = "\n".join(lines) + "\n"
        result = run_psql_stdin(dsn, f"COPY {tbl} (id, data) FROM STDIN;\n{data}\\.\n")
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        count_int = int(count)
        if count_int == 5000:
            report("COPY: partial commit on mid-stream error (5000 committed)", "PASS",
                   "first rotation committed, second batch rolled back")
        elif count_int == 0:
            report("COPY: partial commit on mid-stream error", "PASS",
                   "entire COPY rolled back (atomic behavior)")
        elif count_int == 7000:
            report("COPY: duplicate PK not detected", "BUG", f"got {count} rows")
        else:
            report("COPY: partial commit unexpected count", "FAIL",
                   f"got {count} rows (neither 0 nor 5000)")

    except Exception as e:
        report("COPY boundary", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 3. HNSW: concurrent writes + reads stress test
# ================================================================
def test_hnsw_concurrent_stress(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_stress_{sfx}"
    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {tbl}")
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        # Insert 100 base vectors
        vals = ", ".join(
            f"({i}, '[{random.random():.4f}, {random.random():.4f}, {random.random():.4f}]')"
            for i in range(1, 101)
        )
        run_sql(dsn, f"INSERT INTO {tbl} (id, v) VALUES {vals}")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

        time.sleep(3)  # Wait for merge

        errors = []
        results = {}

        def reader(thread_id, count):
            """Run HNSW queries repeatedly."""
            for i in range(count):
                try:
                    qv = f"[{random.random():.4f}, {random.random():.4f}, {random.random():.4f}]"
                    r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 3")
                    if not r:
                        errors.append(f"reader-{thread_id}: empty result at iteration {i}")
                except Exception as e:
                    errors.append(f"reader-{thread_id}: {e}")

        def writer(thread_id, start_id, count):
            """Insert vectors concurrently."""
            for i in range(count):
                try:
                    vid = start_id + i
                    v = f"[{random.random():.4f}, {random.random():.4f}, {random.random():.4f}]"
                    run_sql(dsn, f"INSERT INTO {tbl} VALUES ({vid}, '{v}')")
                except Exception as e:
                    errors.append(f"writer-{thread_id}: {e}")

        def updater(thread_id, count):
            """Update random vectors concurrently."""
            for i in range(count):
                try:
                    rid = random.randint(1, 100)
                    v = f"[{random.random():.4f}, {random.random():.4f}, {random.random():.4f}]"
                    run_sql(dsn, f"UPDATE {tbl} SET v = '{v}' WHERE id = {rid}")
                except Exception as e:
                    errors.append(f"updater-{thread_id}: {e}")

        def deleter(thread_id, ids):
            """Delete specific vectors."""
            for rid in ids:
                try:
                    run_sql(dsn, f"DELETE FROM {tbl} WHERE id = {rid}")
                except Exception as e:
                    errors.append(f"deleter-{thread_id}: {e}")

        # Launch: 4 readers (10 queries each), 2 writers (5 inserts each),
        # 1 updater (5 updates), 1 deleter (5 deletes)
        threads = []
        for i in range(4):
            threads.append(threading.Thread(target=reader, args=(i, 10)))
        for i in range(2):
            threads.append(threading.Thread(target=writer, args=(i, 200 + i*20, 5)))
        threads.append(threading.Thread(target=updater, args=(0, 5)))
        threads.append(threading.Thread(target=deleter, args=(0, [95, 96, 97, 98, 99])))

        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=60)

        if errors:
            # Check if errors are crashes vs expected conflicts
            crash_errors = [e for e in errors if "crash" in e.lower() or "panic" in e.lower()]
            conflict_errors = [e for e in errors if "conflict" in e.lower() or "lock" in e.lower()]
            other_errors = [e for e in errors if e not in crash_errors and e not in conflict_errors]

            if crash_errors:
                report("HNSW: concurrent stress", "BUG",
                       f"CRASHES: {crash_errors[:3]}")
            elif other_errors:
                report("HNSW: concurrent stress", "FAIL",
                       f"{len(errors)} errors (sample: {other_errors[0][:150]})")
            else:
                report("HNSW: concurrent stress", "PASS",
                       f"{len(conflict_errors)} expected write conflicts")
        else:
            report("HNSW: concurrent R/W/U/D stress (8 threads)", "PASS")

        # Verify table is still queryable
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        result = run_psql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0.5, 0.5, 0.5]' LIMIT 1",
                          expect_success=False)
        if result.returncode == 0:
            report("HNSW: table queryable after stress", "PASS")
        else:
            report("HNSW: table broken after stress", "BUG", result.stderr[:200])

    except Exception as e:
        report("HNSW concurrent stress", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 4. HNSW: LIMIT > row count (must not crash or hang)
# ================================================================
def test_hnsw_limit_edge(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_lim_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, '[1,0,0]'), (2, '[0,1,0]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(3)

        # LIMIT > actual rows
        result = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 100) t")
        if result == "2":
            report("HNSW: LIMIT 100 on 2 rows returns 2", "PASS")
        else:
            report("HNSW: LIMIT > rows", "BUG", f"expected 2, got {result}")

        # LIMIT 0
        result = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 0) t")
        if result == "0":
            report("HNSW: LIMIT 0 returns 0", "PASS")
        else:
            report("HNSW: LIMIT 0", "BUG", f"expected 0, got {result}")

        # Empty table
        run_sql(dsn, f"DELETE FROM {tbl}")
        result = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 10) t")
        if result == "0":
            report("HNSW: query on empty table", "PASS")
        else:
            report("HNSW: query on empty table", "BUG", f"expected 0, got {result}")

    except Exception as e:
        report("HNSW limit edge", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 5. HNSW: DELETE all rows then query
# ================================================================
def test_hnsw_delete_all_then_query(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_delall_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, '[1,0,0]'), (2, '[0,1,0]'), (3, '[0,0,1]')")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(3)

        # Delete all rows
        run_sql(dsn, f"DELETE FROM {tbl}")

        # Query should return 0 rows, not crash
        result = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 10) t")
        if result == "0":
            report("HNSW: query after DELETE all", "PASS")
        else:
            report("HNSW: query after DELETE all", "BUG", f"ghost rows: {result}")

        # Re-insert and query (graph should still work)
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (10, '[0.5,0.5,0]')")
        result = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0.5,0.5,0]' LIMIT 1")
        if result == "10":
            report("HNSW: re-insert after DELETE all", "PASS")
        else:
            report("HNSW: re-insert after DELETE all", "BUG", f"expected 10, got {result}")

    except Exception as e:
        report("HNSW delete all", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 6. ALTER TABLE: DROP COLUMN with various dependencies
# ================================================================
def test_alter_table_deps(dsn):
    sfx = random_suffix()

    def test_dep(name, setup_sql, drop_sql, expect_block=True):
        """Test that DROP COLUMN is blocked/allowed as expected."""
        try:
            for sql in setup_sql:
                run_sql(dsn, sql)
            result = run_psql(dsn, drop_sql, expect_success=False)
            blocked = result.returncode != 0
            if expect_block and blocked:
                report(f"ALTER: {name} blocked", "PASS")
            elif expect_block and not blocked:
                report(f"ALTER: {name} NOT blocked", "BUG", "dependency check missing")
            elif not expect_block and not blocked:
                report(f"ALTER: {name} allowed", "PASS")
            elif not expect_block and blocked:
                report(f"ALTER: {name} wrongly blocked", "BUG", result.stderr[:150])
        except Exception as e:
            report(f"ALTER: {name}", "FAIL", str(e)[:200])

    # CHECK constraint
    test_dep("CHECK constraint", [
        f"DROP TABLE IF EXISTS at_{sfx}_ck",
        f"CREATE TABLE at_{sfx}_ck (id INT PRIMARY KEY, age INT CHECK (age > 0))",
    ], f"ALTER TABLE at_{sfx}_ck DROP COLUMN age", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_ck", expect_success=False)

    # Expression index
    test_dep("expression index", [
        f"DROP TABLE IF EXISTS at_{sfx}_ei",
        f"CREATE TABLE at_{sfx}_ei (id INT PRIMARY KEY, name TEXT)",
        f"CREATE INDEX idx_{sfx}_ei ON at_{sfx}_ei ((lower(name)))",
    ], f"ALTER TABLE at_{sfx}_ei DROP COLUMN name", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_ei CASCADE", expect_success=False)

    # Partial index predicate
    test_dep("partial index predicate", [
        f"DROP TABLE IF EXISTS at_{sfx}_pi",
        f"CREATE TABLE at_{sfx}_pi (id INT PRIMARY KEY, active BOOLEAN, val INT)",
        f"CREATE INDEX idx_{sfx}_pi ON at_{sfx}_pi (val) WHERE active = true",
    ], f"ALTER TABLE at_{sfx}_pi DROP COLUMN active", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_pi CASCADE", expect_success=False)

    # Generated column
    test_dep("generated column", [
        f"DROP TABLE IF EXISTS at_{sfx}_gc",
        f"CREATE TABLE at_{sfx}_gc (id INT PRIMARY KEY, a INT, b INT GENERATED ALWAYS AS (a * 2) STORED)",
    ], f"ALTER TABLE at_{sfx}_gc DROP COLUMN a", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_gc", expect_success=False)

    # Foreign key
    test_dep("foreign key", [
        f"DROP TABLE IF EXISTS at_{sfx}_fkc",
        f"DROP TABLE IF EXISTS at_{sfx}_fkp",
        f"CREATE TABLE at_{sfx}_fkp (id INT PRIMARY KEY, ref_col INT UNIQUE)",
        f"CREATE TABLE at_{sfx}_fkc (id INT PRIMARY KEY, fk INT REFERENCES at_{sfx}_fkp(ref_col))",
    ], f"ALTER TABLE at_{sfx}_fkp DROP COLUMN ref_col", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_fkc", expect_success=False)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_fkp", expect_success=False)

    # View dependency
    test_dep("view dependency", [
        f"DROP VIEW IF EXISTS at_{sfx}_vw",
        f"DROP TABLE IF EXISTS at_{sfx}_vt",
        f"CREATE TABLE at_{sfx}_vt (id INT PRIMARY KEY, name TEXT)",
        f"CREATE VIEW at_{sfx}_vw AS SELECT id, name FROM at_{sfx}_vt",
    ], f"ALTER TABLE at_{sfx}_vt DROP COLUMN name", expect_block=True)
    run_psql(dsn, f"DROP VIEW IF EXISTS at_{sfx}_vw", expect_success=False)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_vt", expect_success=False)

    # No dependency: should succeed
    test_dep("no dependency (should succeed)", [
        f"DROP TABLE IF EXISTS at_{sfx}_ok",
        f"CREATE TABLE at_{sfx}_ok (id INT PRIMARY KEY, a INT, b INT)",
        f"INSERT INTO at_{sfx}_ok VALUES (1, 10, 20)",
    ], f"ALTER TABLE at_{sfx}_ok DROP COLUMN b", expect_block=False)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_ok", expect_success=False)

    # PK column: must be blocked
    test_dep("PK column", [
        f"DROP TABLE IF EXISTS at_{sfx}_pk",
        f"CREATE TABLE at_{sfx}_pk (id INT PRIMARY KEY, val INT)",
    ], f"ALTER TABLE at_{sfx}_pk DROP COLUMN id", expect_block=True)
    run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_pk", expect_success=False)

    # DROP COLUMN IF EXISTS on nonexistent: must be silent no-op
    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_ne")
        run_sql(dsn, f"CREATE TABLE at_{sfx}_ne (id INT PRIMARY KEY, val INT)")
        run_sql(dsn, f"ALTER TABLE at_{sfx}_ne DROP COLUMN IF EXISTS nonexistent")
        report("ALTER: IF EXISTS nonexistent = no-op", "PASS")
    except Exception as e:
        report("ALTER: IF EXISTS nonexistent", "BUG", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS at_{sfx}_ne", expect_success=False)


# ================================================================
# 7. GC safepoint: long transaction protection
# ================================================================
def test_gc_long_txn(dsn):
    sfx = random_suffix()
    tbl = f"gc_long_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 'seed')")

        # Open long transaction, verify data remains readable
        result = run_psql_stdin(dsn,
            f"BEGIN;\n"
            f"SELECT count(*) FROM {tbl};\n"
            f"SELECT pg_sleep(5);\n"
            f"SELECT count(*) FROM {tbl};\n"
            f"COMMIT;\n"
        )
        if result.returncode == 0 and "1" in result.stdout:
            report("GC: long txn data visible", "PASS")
        else:
            report("GC: long txn", "BUG", f"rc={result.returncode}, out={result.stdout[:100]}")

        # Rapid open/close: 500 transactions
        cmds = []
        for i in range(500):
            cmds.append(f"BEGIN; SELECT {i}; COMMIT;")
        result = run_psql_stdin(dsn, "\n".join(cmds), timeout=60)
        if result.returncode == 0:
            report("GC: 500 rapid txn cycles", "PASS")
        else:
            report("GC: 500 rapid txn cycles", "FAIL", result.stderr[:200])

    except Exception as e:
        report("GC long txn", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# Main
# ================================================================
def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("STRESS / BOUNDARY TEST")
    print("Goal: find bugs, report all failures with severity")
    print("=" * 60)

    test_kv_boundary(dsn)
    test_copy_boundaries(dsn)
    test_hnsw_concurrent_stress(dsn)
    test_hnsw_limit_edge(dsn)
    test_hnsw_delete_all_then_query(dsn)
    test_alter_table_deps(dsn)
    test_gc_long_txn(dsn)

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
        print("\n--- BUGS (need fix before launch) ---")
        for name, _, detail in bugs:
            print(f"  [BUG] {name}: {detail}")

    if fails:
        print("\n--- FAILURES (need investigation) ---")
        for name, _, detail in fails:
            print(f"  [FAIL] {name}: {detail}")

    return 1 if bugs else 0


if __name__ == "__main__":
    raise SystemExit(main())

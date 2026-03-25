#!/usr/bin/env python3
"""
Critical boundary tests:
1. GC safepoint: long transaction, cron interaction, data visibility after GC cycles
2. KV region size: exact boundary around 8 MiB guard, protobuf overhead gap

These are the most likely data-loss and crash scenarios.
Server should be started with:
  DB9_GC_LIFE_TIME_SEC=600
  DB9_GC_SAFEPOINT_INTERVAL_SEC=30
  DB9_GC_SAFEPOINT_ENABLED=true
"""

import argparse
import random
import subprocess
import threading
import time


def parse_args():
    parser = argparse.ArgumentParser(description="GC + KV size critical tests")
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


def run_psql_stdin(dsn, input_text, timeout=120):
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=input_text, capture_output=True, text=True, timeout=timeout,
    )


RESULTS = []


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        for line in detail.split("\n"):
            print(f"        {line}")


# ================================================================
# GC TEST 1: Long transaction holds safepoint while other sessions
# update the same rows. Verify snapshot isolation holds across
# multiple GC publisher ticks (30s each).
# ================================================================
def test_gc_long_txn_snapshot(dsn):
    tbl = f"gc_snap_{int(time.time())}"
    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {tbl}")
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, ver INT, data TEXT)")
        # Initial data: ver=1
        for i in range(1, 101):
            run_sql(dsn, f"INSERT INTO {tbl} VALUES ({i}, 1, 'original_{i}')")

        # Connection A: start long txn, read snapshot
        proc_a = subprocess.Popen(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )

        # A: BEGIN and read initial snapshot
        proc_a.stdin.write("BEGIN;\n")
        proc_a.stdin.write(f"SELECT count(*) FROM {tbl} WHERE ver = 1;\n")
        proc_a.stdin.flush()
        time.sleep(1)

        # Connection B: update all rows to ver=2 (creates new MVCC versions)
        for i in range(1, 101):
            run_sql(dsn, f"UPDATE {tbl} SET ver = 2, data = 'updated_{i}' WHERE id = {i}")

        # Connection C: update all rows to ver=3
        for i in range(1, 101):
            run_sql(dsn, f"UPDATE {tbl} SET ver = 3, data = 'updated_again_{i}' WHERE id = {i}")

        # Wait for at least one GC publisher tick (30s) + advancer tick
        print("        waiting 35s for GC publisher tick...")
        time.sleep(35)

        # A: read the same data again — MUST still see ver=1 (snapshot isolation)
        proc_a.stdin.write(f"SELECT count(*) FROM {tbl} WHERE ver = 1;\n")
        proc_a.stdin.write(f"SELECT count(*) FROM {tbl} WHERE ver = 2;\n")
        proc_a.stdin.write(f"SELECT count(*) FROM {tbl} WHERE ver = 3;\n")
        proc_a.stdin.write("COMMIT;\n")
        proc_a.stdin.close()

        stdout, stderr = proc_a.communicate(timeout=30)
        lines = [l.strip() for l in stdout.strip().split("\n") if l.strip()]

        # Expected: first read = 100, second read = 100 (still ver=1), ver2=0, ver3=0
        if len(lines) >= 4:
            initial_count = lines[0]
            snapshot_v1 = lines[1]
            snapshot_v2 = lines[2]
            snapshot_v3 = lines[3]

            if snapshot_v1 == "100" and snapshot_v2 == "0" and snapshot_v3 == "0":
                report("GC: long txn snapshot holds after GC tick", "PASS",
                       f"initial={initial_count}, v1={snapshot_v1}, v2={snapshot_v2}, v3={snapshot_v3}")
            elif snapshot_v1 != "100":
                report("GC: long txn snapshot BROKEN", "BUG",
                       f"MVCC versions GC'd! v1={snapshot_v1} (expected 100), v2={snapshot_v2}, v3={snapshot_v3}\n"
                       f"This means GC advanced past the long transaction's start_ts!")
            else:
                report("GC: long txn snapshot unexpected", "FAIL",
                       f"v1={snapshot_v1}, v2={snapshot_v2}, v3={snapshot_v3}")
        else:
            if "error" in stderr.lower() or "fatal" in stderr.lower():
                report("GC: long txn snapshot", "BUG",
                       f"Transaction errored: {stderr[:300]}")
            else:
                report("GC: long txn snapshot", "FAIL",
                       f"unexpected output: {stdout[:200]}, stderr: {stderr[:200]}")

    except Exception as e:
        report("GC long txn snapshot", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# GC TEST 2: Cron job + long user transaction concurrency
# Schedule a cron job that does a heavy scan, while a user
# session holds a long transaction. Both must be GC-protected.
# ================================================================
def test_gc_cron_and_long_txn(dsn):
    tbl = f"gc_cron_{int(time.time())}"
    try:
        run_sql(dsn, f"DROP TABLE IF EXISTS {tbl}")
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT)")
        for i in range(1, 201):
            run_sql(dsn, f"INSERT INTO {tbl} VALUES ({i}, 'row_{i}')")

        # Schedule cron job that scans the table every minute
        run_sql(dsn, f"""
            SELECT cron.schedule('gc_test_cron', '* * * * *',
                'SELECT count(*) FROM {tbl}')
        """)

        # User: open long transaction and hold
        proc = subprocess.Popen(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        proc.stdin.write("BEGIN;\n")
        proc.stdin.write(f"SELECT count(*) FROM {tbl};\n")
        proc.stdin.flush()
        time.sleep(1)

        # Another session: update all rows
        for i in range(1, 201):
            run_sql(dsn, f"UPDATE {tbl} SET data = 'modified_{i}' WHERE id = {i}")

        # Wait for GC tick + cron execution
        print("        waiting 35s for GC tick + cron run...")
        time.sleep(35)

        # User: verify snapshot still intact
        proc.stdin.write(f"SELECT count(*) FROM {tbl} WHERE data LIKE 'row_%';\n")
        proc.stdin.write("COMMIT;\n")
        proc.stdin.close()

        stdout, stderr = proc.communicate(timeout=30)
        lines = [l.strip() for l in stdout.strip().split("\n") if l.strip()]

        # Cleanup cron
        run_psql(dsn, "SELECT cron.unschedule('gc_test_cron')", expect_success=False)

        if len(lines) >= 2:
            initial = lines[0]
            after_gc = lines[1]
            if after_gc == "200":
                report("GC: cron + long txn coexist", "PASS",
                       f"initial={initial}, after_gc_tick={after_gc}")
            else:
                report("GC: cron + long txn snapshot broken", "BUG",
                       f"Expected 200 original rows, got {after_gc}.\n"
                       f"Cron or GC interfered with user txn snapshot!")
        else:
            report("GC: cron + long txn", "FAIL",
                   f"stdout: {stdout[:200]}, stderr: {stderr[:200]}")

    except Exception as e:
        report("GC cron + long txn", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, "SELECT cron.unschedule('gc_test_cron')", expect_success=False)
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# GC TEST 3: Abrupt disconnect with uncommitted data
# Verify quarantine prevents GC from advancing too quickly
# ================================================================
def test_gc_abrupt_disconnect(dsn):
    tbl = f"gc_disc_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 'original')")

        # Open txn, update data, then disconnect without commit
        proc = subprocess.Popen(
            ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        proc.stdin.write("BEGIN;\n")
        proc.stdin.write(f"UPDATE {tbl} SET data = 'will_not_commit' WHERE id = 1;\n")
        proc.stdin.flush()
        time.sleep(1)

        # Kill the connection (simulate crash)
        proc.kill()
        proc.wait()

        time.sleep(2)

        # On a new connection: data should be 'original' (rolled back)
        data = run_sql(dsn, f"SELECT data FROM {tbl} WHERE id = 1")
        if data == "original":
            report("GC: abrupt disconnect data rolled back", "PASS")
        elif data == "will_not_commit":
            report("GC: uncommitted data visible", "BUG",
                   "Uncommitted write survived disconnect!")
        else:
            report("GC: abrupt disconnect", "BUG", f"unexpected data: {data}")

        # Wait for GC tick, then verify again
        print("        waiting 35s for post-disconnect GC tick...")
        time.sleep(35)
        data = run_sql(dsn, f"SELECT data FROM {tbl} WHERE id = 1")
        if data == "original":
            report("GC: data still correct after GC tick post-disconnect", "PASS")
        else:
            report("GC: data corrupted after GC post-disconnect", "BUG",
                   f"expected 'original', got '{data}'")

    except Exception as e:
        report("GC abrupt disconnect", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# GC TEST 4: Multiple concurrent long transactions
# Each holds a different snapshot. Verify all are protected.
# ================================================================
def test_gc_concurrent_long_txns(dsn):
    tbl = f"gc_multi_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, ver INT)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 0)")

        # Open 3 transactions at different points in time
        procs = []
        for i in range(3):
            # Update to ver=i+1 before opening txn i
            run_sql(dsn, f"UPDATE {tbl} SET ver = {i + 1} WHERE id = 1")
            time.sleep(0.5)

            proc = subprocess.Popen(
                ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True,
            )
            proc.stdin.write("BEGIN;\n")
            proc.stdin.write(f"SELECT ver FROM {tbl} WHERE id = 1;\n")
            proc.stdin.flush()
            procs.append(proc)

        # Update to ver=99
        run_sql(dsn, f"UPDATE {tbl} SET ver = 99 WHERE id = 1")

        # Wait for GC tick
        print("        waiting 35s for GC tick with 3 concurrent txns...")
        time.sleep(35)

        # Each txn should still see its own snapshot
        expected = [1, 2, 3]
        all_ok = True
        for i, proc in enumerate(procs):
            proc.stdin.write(f"SELECT ver FROM {tbl} WHERE id = 1;\n")
            proc.stdin.write("COMMIT;\n")
            proc.stdin.close()
            stdout, stderr = proc.communicate(timeout=15)
            lines = [l.strip() for l in stdout.strip().split("\n") if l.strip()]
            if len(lines) >= 2:
                snapshot_ver = lines[0]  # from initial read
                post_gc_ver = lines[1]   # from second read after GC tick
                if post_gc_ver == str(expected[i]):
                    pass  # good
                else:
                    report(f"GC: concurrent txn {i} snapshot broken", "BUG",
                           f"expected ver={expected[i]}, got {post_gc_ver} after GC tick")
                    all_ok = False
            else:
                report(f"GC: concurrent txn {i}", "FAIL",
                       f"stdout: {stdout[:100]}, stderr: {stderr[:100]}")
                all_ok = False

        if all_ok:
            report("GC: 3 concurrent long txns all hold snapshots", "PASS")

    except Exception as e:
        report("GC concurrent long txns", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# KV TEST 1: Exact 8 MiB boundary probing
# Guard checks key.len() + value.len() > 8388608
# Key overhead ~31 bytes for int PK. Try values that
# just barely pass the guard but might fail at TiKV.
# ================================================================
def test_kv_exact_boundary(dsn):
    tbl = f"kv_exact_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, data TEXT NOT NULL)")

        # The guard limit is 8388608 bytes for key+value combined
        # Key ~31 bytes, value encoding overhead ~20 bytes
        # So max TEXT payload ≈ 8388608 - 31 - 20 = 8388557

        # Test 1: 8MB - 1KB (well under, must succeed)
        size = 8388608 - 1024
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, repeat('a', {size}))")
        report("KV boundary: 8MiB - 1KB", "PASS")

        # Test 2: 8MB - 100 bytes (very close to guard limit after key/value overhead)
        size = 8388608 - 100
        result = run_psql(dsn, f"INSERT INTO {tbl} VALUES (2, repeat('b', {size}))", expect_success=False)
        if result.returncode == 0:
            # Passed guard — might fail at TiKV commit if raft overhead pushes it over
            report("KV boundary: 8MiB - 100B passed guard", "PASS",
                   "accepted by guard (key+value ≈ 8MiB, but TiKV commit may fail)")
        elif "value too large" in result.stderr.lower():
            report("KV boundary: 8MiB - 100B caught by guard", "PASS")
        elif "raft" in result.stderr.lower():
            report("KV boundary: 8MiB - 100B LEAKED past guard to TiKV", "BUG",
                   f"Guard should have caught this! Error: {result.stderr[:200]}")
        else:
            report("KV boundary: 8MiB - 100B", "FAIL", result.stderr[:200])

        # Test 3: exactly 8388608 TEXT bytes (will exceed guard with key overhead)
        size = 8388608
        result = run_psql(dsn, f"INSERT INTO {tbl} VALUES (3, repeat('c', {size}))", expect_success=False)
        if result.returncode != 0 and "value too large" in result.stderr.lower():
            report("KV boundary: exactly 8MiB TEXT rejected by guard", "PASS")
        elif result.returncode != 0 and "raft" in result.stderr.lower():
            report("KV boundary: 8MiB TEXT leaked to TiKV", "BUG",
                   "Guard failed — RaftEntryTooLarge at commit time")
        elif result.returncode == 0:
            report("KV boundary: 8MiB TEXT accepted", "BUG",
                   "8MiB TEXT + key overhead should exceed guard limit!")
        else:
            report("KV boundary: 8MiB TEXT", "FAIL", result.stderr[:200])

        # Test 4: Multi-row transaction total > 100MB (TiKV txn-total-size-limit)
        # Each row ≈ 5MB, 25 rows = 125MB > 100MB limit
        run_sql(dsn, f"DELETE FROM {tbl}")
        result = run_psql_stdin(dsn,
            "BEGIN;\n" +
            "\n".join(f"INSERT INTO {tbl} VALUES ({100+i}, repeat('d', 5000000));"
                      for i in range(25)) +
            "\nCOMMIT;\n"
        )
        if result.returncode != 0:
            stderr = result.stderr.lower()
            if "transaction is too large" in stderr or "txn_total_size" in stderr:
                report("KV: multi-row txn > 100MB caught by TiKV", "PASS",
                       "TiKV rejected oversized transaction")
            elif "value too large" in stderr:
                report("KV: multi-row txn caught by row guard", "PASS")
            else:
                report("KV: multi-row txn > 100MB", "FAIL",
                       f"rejected but unexpected error: {result.stderr[:200]}")
        else:
            # This is actually a finding — no transaction-level guard
            count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
            if count == "25":
                report("KV: multi-row txn > 100MB ACCEPTED", "FAIL",
                       "No transaction-level size guard! 125MB txn committed.\n"
                       "TiKV txn-total-size-limit may be higher than 100MB in this cluster.")
            else:
                report("KV: multi-row txn", "FAIL", f"partial commit: {count} rows")

    except Exception as e:
        report("KV exact boundary", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# KV TEST 2: Index entry size
# If a table has a B-tree index on a TEXT column, inserting a
# large text value creates a large index entry too.
# ================================================================
def test_kv_index_entry_size(dsn):
    tbl = f"kv_idx_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, indexed_text TEXT)")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} (indexed_text)")

        # Insert a large indexed text (7MB) — the index entry encodes the full text
        result = run_psql(dsn,
            f"INSERT INTO {tbl} VALUES (1, repeat('x', 7000000))",
            expect_success=False)
        if result.returncode == 0:
            report("KV: 7MB indexed text accepted", "PASS",
                   "row + index entry both under guard limit")
        elif "value too large" in result.stderr.lower():
            report("KV: 7MB indexed text rejected by guard", "PASS",
                   "index entry exceeded limit (expected for large indexed values)")
        elif "raft" in result.stderr.lower():
            report("KV: 7MB indexed text leaked to TiKV", "BUG",
                   f"Guard missed index entry! {result.stderr[:200]}")
        else:
            report("KV: 7MB indexed text", "FAIL", result.stderr[:200])

    except Exception as e:
        report("KV index entry", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("CRITICAL: GC SAFEPOINT + KV SIZE BOUNDARY TESTS")
    print("Server config: gc_life_time=600s, gc_interval=30s")
    print("=" * 60)

    test_gc_long_txn_snapshot(dsn)
    test_gc_cron_and_long_txn(dsn)
    test_gc_abrupt_disconnect(dsn)
    test_gc_concurrent_long_txns(dsn)
    test_kv_exact_boundary(dsn)
    test_kv_index_entry_size(dsn)

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
        print("\n--- BUGS (CRITICAL — fix before launch) ---")
        for name, _, detail in bugs:
            print(f"  [BUG] {name}:")
            for line in detail.split("\n"):
                print(f"        {line}")

    if fails:
        print("\n--- FAILURES (investigate) ---")
        for name, _, detail in fails:
            print(f"  [FAIL] {name}:")
            for line in detail.split("\n"):
                print(f"        {line}")

    return 1 if bugs else 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""
GC safepoint critical tests: long txn snapshot protection across GC ticks.
Server MUST be started with DB9_GC_SAFEPOINT_INTERVAL_SEC=30.
"""

import argparse
import subprocess
import time
import threading


def parse_args():
    parser = argparse.ArgumentParser()
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


RESULTS = []


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        for line in detail.split("\n"):
            print(f"        {line}")


def test_gc_long_txn_snapshot(dsn):
    """Long txn holds snapshot while other sessions update rows.
    Wait >30s for GC tick, then verify snapshot isolation still holds."""
    tbl = f"gc_snap_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, ver INT)")
        for i in range(1, 51):
            run_sql(dsn, f"INSERT INTO {tbl} VALUES ({i}, 1)")

        # Session A: long transaction using a single psql invocation
        # We use a single -c with pg_sleep to keep the txn open
        # But we need multi-statement... use -f - with careful handling

        # Approach: use two separate psql calls with the same state via
        # explicit serializable snapshot.
        # Actually simpler: run the long txn in a thread.

        snapshot_result = [None]
        error_result = [None]

        def long_txn():
            try:
                # Single psql session: BEGIN, read, sleep 40s, read again, COMMIT
                result = subprocess.run(
                    ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
                    input=(
                        "BEGIN;\n"
                        f"SELECT count(*) FROM {tbl} WHERE ver = 1;\n"
                        "SELECT pg_sleep(40);\n"
                        f"SELECT count(*) FROM {tbl} WHERE ver = 1;\n"
                        "COMMIT;\n"
                    ),
                    capture_output=True, text=True, timeout=120,
                )
                snapshot_result[0] = result
            except Exception as e:
                error_result[0] = str(e)

        t = threading.Thread(target=long_txn)
        t.start()

        # Wait for txn to start
        time.sleep(3)

        # Session B: update all rows to ver=2
        for i in range(1, 51):
            run_sql(dsn, f"UPDATE {tbl} SET ver = 2 WHERE id = {i}")

        # Session C: update all rows to ver=3
        for i in range(1, 51):
            run_sql(dsn, f"UPDATE {tbl} SET ver = 3 WHERE id = {i}")

        print("        waiting for long txn (40s including GC tick)...")
        t.join(timeout=120)

        if error_result[0]:
            report("GC: long txn snapshot", "FAIL", f"thread error: {error_result[0]}")
            return

        result = snapshot_result[0]
        if not result:
            report("GC: long txn snapshot", "FAIL", "no result from thread")
            return

        stdout = result.stdout.strip()
        stderr = result.stderr.strip()
        lines = [l for l in stdout.split("\n") if l.strip() and l.strip() != ""]

        # Filter out pg_sleep empty result
        data_lines = [l.strip() for l in lines if l.strip() and not l.strip().startswith("")]

        # Expected: "50" (initial read), "" (pg_sleep), "50" (post-GC read)
        counts = [l.strip() for l in lines if l.strip().isdigit()]

        if len(counts) >= 2:
            initial = counts[0]
            post_gc = counts[1]
            if post_gc == "50":
                report("GC: long txn snapshot holds after 40s (GC tick crossed)", "PASS",
                       f"initial={initial}, post_gc={post_gc}")
            else:
                report("GC: SNAPSHOT BROKEN — GC advanced past active txn", "BUG",
                       f"initial={initial}, post_gc={post_gc} (expected 50)\n"
                       f"MVCC versions were garbage collected!")
        else:
            if "error" in stderr.lower():
                report("GC: long txn errored", "BUG",
                       f"Txn failed after GC tick: {stderr[:300]}")
            else:
                report("GC: long txn unexpected output", "FAIL",
                       f"stdout lines: {lines}, stderr: {stderr[:200]}")

    except Exception as e:
        report("GC long txn snapshot", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_gc_concurrent_snapshots(dsn):
    """3 transactions opened at different ver levels, all survive GC tick."""
    tbl = f"gc_multi_{int(time.time())}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, ver INT)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 0)")

        snapshot_results = [None, None, None]

        def open_txn_and_hold(idx, expected_ver, sleep_sec):
            try:
                result = subprocess.run(
                    ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
                    input=(
                        "BEGIN;\n"
                        f"SELECT ver FROM {tbl} WHERE id = 1;\n"
                        f"SELECT pg_sleep({sleep_sec});\n"
                        f"SELECT ver FROM {tbl} WHERE id = 1;\n"
                        "COMMIT;\n"
                    ),
                    capture_output=True, text=True, timeout=120,
                )
                snapshot_results[idx] = result
            except Exception as e:
                snapshot_results[idx] = str(e)

        # Update to ver=1, open txn A
        run_sql(dsn, f"UPDATE {tbl} SET ver = 1 WHERE id = 1")
        t0 = threading.Thread(target=open_txn_and_hold, args=(0, 1, 40))
        t0.start()
        time.sleep(1)

        # Update to ver=2, open txn B
        run_sql(dsn, f"UPDATE {tbl} SET ver = 2 WHERE id = 1")
        t1 = threading.Thread(target=open_txn_and_hold, args=(1, 2, 38))
        t1.start()
        time.sleep(1)

        # Update to ver=3, open txn C
        run_sql(dsn, f"UPDATE {tbl} SET ver = 3 WHERE id = 1")
        t2 = threading.Thread(target=open_txn_and_hold, args=(2, 3, 36))
        t2.start()

        # Update to ver=99 (latest)
        time.sleep(1)
        run_sql(dsn, f"UPDATE {tbl} SET ver = 99 WHERE id = 1")

        print("        waiting for 3 concurrent txns (40s)...")
        for t in [t0, t1, t2]:
            t.join(timeout=120)

        expected = [1, 2, 3]
        all_ok = True
        for i in range(3):
            r = snapshot_results[i]
            if isinstance(r, str):
                report(f"GC: concurrent txn {i}", "FAIL", r[:200])
                all_ok = False
                continue

            lines = [l.strip() for l in r.stdout.split("\n") if l.strip().isdigit()]
            if len(lines) >= 2:
                post_gc_ver = lines[-1]  # last numeric value = second SELECT
                if post_gc_ver == str(expected[i]):
                    pass
                else:
                    report(f"GC: concurrent txn {i} SNAPSHOT BROKEN", "BUG",
                           f"expected ver={expected[i]}, got {post_gc_ver}")
                    all_ok = False
            else:
                stderr = r.stderr[:200] if r.stderr else ""
                report(f"GC: concurrent txn {i}", "FAIL",
                       f"output: {r.stdout[:100]}, err: {stderr}")
                all_ok = False

        if all_ok:
            report("GC: 3 concurrent txns all hold snapshots after GC tick", "PASS")

    except Exception as e:
        report("GC concurrent snapshots", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("GC SAFEPOINT CRITICAL TESTS")
    print("Requires: DB9_GC_SAFEPOINT_INTERVAL_SEC=30")
    print("Each test waits >30s for GC tick to cross")
    print("=" * 60)

    test_gc_long_txn_snapshot(dsn)
    test_gc_concurrent_snapshots(dsn)

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

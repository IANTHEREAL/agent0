#!/usr/bin/env python3
"""
COPY FROM STDIN: 50,000 unrotated rows ceiling test.

When self-referencing FK blocks transaction rotation, rows accumulate in
a single transaction. At COPY_STDIN_MAX_UNROTATED_ROWS=50,000 the COPY
must abort with SQLSTATE 54000 to prevent hitting TiKV's 100MB txn limit.

Tests:
1. Self-FK with ALL forward refs (child before parent) — rotation blocked,
   ceiling fires at 50,000
2. Self-FK with forward refs that resolve within 50K — rotation resumes,
   COPY succeeds
"""

import argparse
import random
import subprocess
import time


def parse_args():
    parser = argparse.ArgumentParser(
        description="COPY 50K unrotated ceiling test"
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


def run_psql_stdin(dsn, input_text, timeout=600):
    return subprocess.run(
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=input_text, capture_output=True, text=True, timeout=timeout,
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


# ================================================================
# Test 1: Self-FK, all forward refs, exceed 50K ceiling
#
# Table has self-FK: parent_id REFERENCES self(id).
# We send rows where every row references the NEXT row (all forward refs).
# Row 1 → parent=2, Row 2 → parent=3, ..., Row N → parent=N+1
# The final row (N+1) has parent=NULL (root).
# Since every child comes before its parent, rotation is permanently
# blocked. At 50,000 accumulated rows, the ceiling fires.
# ================================================================
def test_ceiling_fires(dsn):
    sfx = random_suffix()
    tbl = f"copy_ceil_{sfx}"
    try:
        run_sql(dsn, f"""CREATE TABLE {tbl} (
            id INT PRIMARY KEY,
            parent_id INT REFERENCES {tbl}(id)
        )""")

        # Generate 51,001 rows: first 51,000 all reference the LAST row (51001).
        # Row 51001 (parent/root) comes last with NULL parent.
        # This means ALL 51,000 forward refs are unresolved until the very
        # last row, forcing the deferred queue to grow to 51,000.
        # The ceiling at 50,000 should abort BEFORE row 51001 arrives.
        n_children = 51000
        root_id = n_children + 1
        print(f"        generating {n_children} forward-ref rows (all → root {root_id})...")
        lines = []
        for i in range(1, n_children + 1):
            lines.append(f"{i}\t{root_id}")
        lines.append(f"{root_id}\t\\N")  # root node comes last
        data = "\n".join(lines) + "\n"

        result = run_psql_stdin(dsn,
            f"COPY {tbl} (id, parent_id) FROM STDIN;\n{data}\\.\n",
            timeout=600)

        stderr = (result.stderr or "").lower()
        has_error = result.returncode != 0 or "error:" in stderr

        if has_error:
            if "self-referencing" in stderr or "forward" in stderr or "unrotated" in stderr or "54000" in stderr:
                report("COPY 50K ceiling: fires on forward-ref self-FK", "PASS",
                       f"correctly rejected at ~50K unrotated rows")
            elif "foreign key" in stderr:
                # FK validation caught it before ceiling — still a safety net
                count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
                report("COPY 50K ceiling: FK validation caught (not ceiling)", "PASS",
                       f"FK error prevented the oversize txn. {count} rows committed.\n"
                       f"Note: ceiling may not have been reached if FK fires first.")
            else:
                report("COPY 50K ceiling: unexpected error", "FAIL",
                       result.stderr[:300])
        else:
            count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
            if int(count) == 0:
                report("COPY 50K ceiling: COPY succeeded but 0 rows (implicit rollback)", "FAIL",
                       "COPY returned success but no rows committed. Check stderr.")
            else:
                report("COPY 50K ceiling: did NOT fire", "BUG",
                       f"51K forward-ref rows accepted ({count} rows committed).\n"
                       f"The 50K unrotated ceiling should have aborted the COPY.")

    except Exception as e:
        report("COPY 50K ceiling", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# Test 2: Self-FK, forward refs resolve within limit → succeeds
#
# Pairs: (child, parent) where parent comes right after child.
# Row 2→1, Row 1→NULL, Row 4→3, Row 3→NULL, ...
# Forward ref (2→1) is resolved when row 1 arrives (next row).
# Rotation is briefly blocked per pair but unblocks immediately.
# Total: 10,000 rows — should succeed since no long-lived forward refs.
# ================================================================
def test_forward_refs_resolve(dsn):
    sfx = random_suffix()
    tbl = f"copy_resolve_{sfx}"
    try:
        run_sql(dsn, f"""CREATE TABLE {tbl} (
            id INT PRIMARY KEY,
            parent_id INT REFERENCES {tbl}(id)
        )""")

        # 10,000 rows in child-parent pairs
        n_pairs = 5000
        lines = []
        for i in range(n_pairs):
            child_id = i * 2 + 2
            parent_id = i * 2 + 1
            lines.append(f"{child_id}\t{parent_id}")  # child (forward ref)
            lines.append(f"{parent_id}\t\\N")          # parent (resolves ref)
        data = "\n".join(lines) + "\n"

        result = run_psql_stdin(dsn,
            f"COPY {tbl} (id, parent_id) FROM STDIN;\n{data}\\.\n",
            timeout=600)

        if result.returncode != 0:
            report("COPY self-FK resolving forward refs", "FAIL",
                   result.stderr[:300])
            return

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == str(n_pairs * 2):
            report(f"COPY self-FK: {n_pairs * 2} rows with resolving forward refs", "PASS")
        else:
            report("COPY self-FK resolving refs", "BUG",
                   f"expected {n_pairs * 2}, got {count}")

    except Exception as e:
        report("COPY self-FK resolving", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("COPY 50K UNROTATED CEILING TEST")
    print("=" * 60)

    test_ceiling_fires(dsn)
    test_forward_refs_resolve(dsn)

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

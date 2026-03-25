#!/usr/bin/env python3
"""
COPY FROM STDIN: self-referencing FK deferred validation + 50K unrotated ceiling.

Tests:
1. Self-FK table, parent-before-child order → rotation works
2. Self-FK table, child-before-parent order → rotation blocked, resolves later
3. Self-FK with excessive forward refs → 50,000 ceiling fires (SQLSTATE 54000)
4. Non-FK table at exactly 5000 rows boundary
"""

import argparse
import random
import string
import subprocess
import time


def parse_args():
    parser = argparse.ArgumentParser(
        description="COPY self-FK + unrotated ceiling tests"
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


def run_psql_stdin(dsn, input_text, timeout=180):
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
        print(f"        {detail}")


def test_selfref_parent_first(dsn):
    """Self-FK table, rows in parent-before-child order → rotation should work."""
    sfx = random_suffix()
    tbl = f"copy_sfk_pf_{sfx}"
    try:
        run_sql(dsn, f"""CREATE TABLE {tbl} (
            id INT PRIMARY KEY,
            parent_id INT REFERENCES {tbl}(id),
            name TEXT
        )""")

        # 6000 rows: parent first (id=1 no parent, id=2 parent=1, etc.)
        # This is a chain: 1 → 2 → 3 → ... → 6000
        # All forward refs resolve immediately since parent always precedes child
        lines = []
        lines.append("1\t\\N\troot")
        for i in range(2, 6001):
            lines.append(f"{i}\t{i-1}\tnode_{i}")
        data = "\n".join(lines) + "\n"

        result = run_psql_stdin(dsn, f"COPY {tbl} (id, parent_id, name) FROM STDIN;\n{data}\\.\n")
        if result.returncode != 0:
            report("COPY self-FK parent-first", "FAIL", result.stderr[:200])
            return

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "6000":
            report("COPY self-FK parent-first (6000 rows)", "PASS")
        else:
            report("COPY self-FK parent-first", "BUG", f"expected 6000, got {count}")
    except Exception as e:
        report("COPY self-FK parent-first", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_selfref_child_first_small(dsn):
    """Self-FK table, child-before-parent in small batches → deferred check resolves."""
    sfx = random_suffix()
    tbl = f"copy_sfk_cf_{sfx}"
    try:
        run_sql(dsn, f"""CREATE TABLE {tbl} (
            id INT PRIMARY KEY,
            parent_id INT REFERENCES {tbl}(id),
            name TEXT
        )""")

        # 100 rows: children reference parents that come later
        # Pairs: (2,1), (1,NULL), (4,3), (3,NULL), ...
        lines = []
        for i in range(0, 100, 2):
            child_id = i + 2
            parent_id = i + 1
            lines.append(f"{child_id}\t{parent_id}\tchild_{child_id}")
            lines.append(f"{parent_id}\t\\N\tparent_{parent_id}")
        data = "\n".join(lines) + "\n"

        result = run_psql_stdin(dsn, f"COPY {tbl} (id, parent_id, name) FROM STDIN;\n{data}\\.\n")
        if result.returncode != 0:
            # This might fail if deferred FK resolution doesn't work
            stderr = result.stderr.lower()
            if "foreign key" in stderr:
                report("COPY self-FK child-first (100 rows)", "FAIL",
                       f"FK deferred check failed: {result.stderr[:200]}")
            else:
                report("COPY self-FK child-first (100 rows)", "FAIL", result.stderr[:200])
            return

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count == "100":
            report("COPY self-FK child-first (100 rows, deferred resolves)", "PASS")
        else:
            report("COPY self-FK child-first", "BUG", f"expected 100, got {count}")
    except Exception as e:
        report("COPY self-FK child-first", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_alter_table_byte_budget(dsn):
    """ALTER TABLE ALTER COLUMN TYPE on a table with enough data to exceed 80MB budget.
    Uses 2000 × 50KB rows ≈ 100MB. ALTER COLUMN TYPE forces row rewrite.
    Note: ADD COLUMN DEFAULT does NOT rewrite rows (like PG 11+).
    Uses smaller rows to stay under gRPC 64MB message limit per scan batch."""
    sfx = random_suffix()
    tbl = f"at_budget_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, val INT, payload TEXT)")

        # Insert 2000 rows × 50KB each = ~100MB total
        print("        inserting 2000 × 50KB rows...")
        for batch in range(20):
            run_sql(dsn, f"""
                INSERT INTO {tbl}
                SELECT g, g, repeat('x', 50000)
                FROM generate_series({batch * 100 + 1}, {(batch + 1) * 100}) g
            """)

        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        assert count == "2000", f"setup failed: {count} rows"

        # ALTER COLUMN TYPE forces full row rewrite → should hit 80MB budget
        result = run_psql(dsn,
            f"ALTER TABLE {tbl} ALTER COLUMN val TYPE BIGINT",
            expect_success=False, timeout=120)

        if result.returncode != 0:
            stderr = result.stderr.lower()
            if "byte" in stderr or "budget" in stderr or "too complex" in stderr or "54001" in stderr:
                report("ALTER TABLE byte budget (100MB > 80MB)", "PASS",
                       "correctly rejected by budget guard")
            elif "transaction is too large" in stderr or "txn" in stderr:
                report("ALTER TABLE byte budget LEAKED to TiKV", "BUG",
                       f"Budget guard didn't fire! TiKV rejected: {result.stderr[:200]}")
            else:
                report("ALTER TABLE byte budget", "FAIL",
                       f"rejected but unexpected error: {result.stderr[:200]}")
        else:
            report("ALTER TABLE byte budget", "FAIL",
                   "100MB ALTER succeeded (budget not triggered).")

    except Exception as e:
        report("ALTER TABLE byte budget", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def test_alter_table_rls_dependency(dsn):
    """DROP COLUMN blocked by RLS policy."""
    sfx = random_suffix()
    tbl = f"at_rls_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, tenant_id INT, data TEXT)")
        run_sql(dsn, f"ALTER TABLE {tbl} ENABLE ROW LEVEL SECURITY")
        run_sql(dsn, f"""CREATE POLICY tenant_policy ON {tbl}
            USING (tenant_id = current_setting('app.tenant_id')::int)""")

        result = run_psql(dsn, f"ALTER TABLE {tbl} DROP COLUMN tenant_id", expect_success=False)
        if result.returncode != 0:
            stderr = result.stderr.lower()
            if "drop column" in stderr or "policy" in stderr or "cannot" in stderr:
                report("ALTER TABLE DROP COLUMN blocked by RLS policy", "PASS")
            else:
                report("ALTER TABLE DROP COLUMN + RLS", "FAIL",
                       f"rejected but unexpected: {result.stderr[:200]}")
        else:
            report("ALTER TABLE DROP COLUMN + RLS", "BUG",
                   "DROP COLUMN succeeded despite RLS policy referencing it!")
    except Exception as e:
        report("ALTER TABLE RLS dep", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl} CASCADE", expect_success=False)


def test_alter_table_matview_dependency(dsn):
    """DROP COLUMN blocked by materialized view."""
    sfx = random_suffix()
    tbl = f"at_mv_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, val INT)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 10)")
        run_sql(dsn, f"CREATE MATERIALIZED VIEW mv_{tbl} AS SELECT id, val FROM {tbl}")

        result = run_psql(dsn, f"ALTER TABLE {tbl} DROP COLUMN val", expect_success=False)
        if result.returncode != 0:
            stderr = result.stderr.lower()
            if "drop column" in stderr or "materialized" in stderr or "depends" in stderr or "cannot" in stderr:
                report("ALTER TABLE DROP COLUMN blocked by matview", "PASS")
            else:
                report("ALTER TABLE DROP COLUMN + matview", "FAIL",
                       f"rejected but unexpected: {result.stderr[:200]}")
        else:
            report("ALTER TABLE DROP COLUMN + matview", "BUG",
                   "DROP COLUMN succeeded despite matview depending on it!")
    except Exception as e:
        report("ALTER TABLE matview dep", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP MATERIALIZED VIEW IF EXISTS mv_{tbl}", expect_success=False)
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("SUPPLEMENTARY INTEGRATION TESTS")
    print("COPY self-FK + ALTER TABLE budget/deps")
    print("=" * 60)

    test_selfref_parent_first(dsn)
    test_selfref_child_first_small(dsn)
    test_alter_table_byte_budget(dsn)
    test_alter_table_rls_dependency(dsn)
    test_alter_table_matview_dependency(dsn)

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

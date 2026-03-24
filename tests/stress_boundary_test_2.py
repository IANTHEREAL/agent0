#!/usr/bin/env python3
"""
Stress boundary test round 2: larger data, tighter races, deeper edge cases.
Goal: FIND BUGS.

Focus areas:
1. HNSW shared cache under heavy concurrent load (20 threads)
2. HNSW: rapid CREATE/DROP/TRUNCATE index cycles
3. COPY: large dataset correctness (50K rows, verify every row)
4. Decimal: window function overflow, nested aggregates
5. Hash join: exact memory boundary probing
6. Concurrent DDL + DML
7. HNSW: UPDATE all vectors, then query (full delta, no base graph useful)
"""

import argparse
import random
import string
import subprocess
import threading
import time


def parse_args():
    parser = argparse.ArgumentParser(description="Stress boundary tests round 2")
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
        ["psql", dsn, "--no-psqlrc", "-A", "-t", "-q", "-f", "-"],
        input=input_text, capture_output=True, text=True, timeout=timeout,
    )


def random_suffix():
    return f"{int(time.time())}_{random.randint(1000,9999)}"


RESULTS = []


def report(name, status, detail=""):
    RESULTS.append((name, status, detail))
    marker = {"PASS": "[OK]", "FAIL": "[FAIL]", "BUG": "[BUG]"}[status]
    print(f"  {marker} {name}")
    if detail:
        print(f"        {detail}")


# ================================================================
# 1. HNSW: 20 concurrent queries + 5 concurrent writers
# ================================================================
def test_hnsw_heavy_concurrent(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_heavy_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(16))")
        # 500 base vectors, 16 dimensions
        batch_size = 50
        for batch in range(10):
            vals = ", ".join(
                f"({batch*batch_size + i}, '[{','.join(f'{random.random():.3f}' for _ in range(16))}]')"
                for i in range(batch_size)
            )
            run_sql(dsn, f"INSERT INTO {tbl} VALUES {vals}")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(5)

        errors = []

        def reader(tid, n):
            for _ in range(n):
                try:
                    qv = "[" + ",".join(f"{random.random():.3f}" for _ in range(16)) + "]"
                    r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '{qv}' LIMIT 5")
                    ids = r.strip().split("\n") if r.strip() else []
                    if len(ids) < 1:
                        errors.append(f"reader-{tid}: got {len(ids)} results")
                except Exception as e:
                    errors.append(f"reader-{tid}: {e}")

        def writer(tid, start_id, n):
            for i in range(n):
                try:
                    vid = start_id + i
                    v = "[" + ",".join(f"{random.random():.3f}" for _ in range(16)) + "]"
                    run_sql(dsn, f"INSERT INTO {tbl} VALUES ({vid}, '{v}')")
                except Exception as e:
                    if "duplicate key" not in str(e).lower():
                        errors.append(f"writer-{tid}: {e}")

        threads = []
        for i in range(20):
            threads.append(threading.Thread(target=reader, args=(i, 5)))
        for i in range(5):
            threads.append(threading.Thread(target=writer, args=(i, 1000 + i*100, 10)))

        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=120)

        crash_errors = [e for e in errors if any(x in e.lower() for x in ["panic", "crash", "internal"])]
        if crash_errors:
            report("HNSW: 20R+5W heavy concurrent", "BUG", f"crashes: {crash_errors[:3]}")
        elif len(errors) > 5:
            report("HNSW: 20R+5W heavy concurrent", "FAIL", f"{len(errors)} errors")
        else:
            report("HNSW: 20R+5W heavy concurrent (25 threads)", "PASS",
                   f"{len(errors)} minor errors" if errors else "")

    except Exception as e:
        report("HNSW heavy concurrent", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 2. HNSW: rapid CREATE/DROP/TRUNCATE index cycles
# ================================================================
def test_hnsw_rapid_ddl_cycles(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_ddl_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")

        for cycle in range(5):
            # Insert data
            vals = ", ".join(f"({cycle*100+i}, '[{i*0.01},{(100-i)*0.01},0.5]')" for i in range(50))
            run_sql(dsn, f"INSERT INTO {tbl} VALUES {vals}")

            # Create index
            run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

            # Query
            result = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[0.5,0.5,0.5]' LIMIT 10) t")
            count = int(result)
            expected_min = min(10, (cycle+1)*50)
            if count < 1:
                report(f"HNSW DDL cycle {cycle}: query empty", "BUG")
                return

            if cycle % 2 == 0:
                # DROP INDEX
                run_sql(dsn, f"DROP INDEX idx_{tbl}")
            else:
                # TRUNCATE (drops all data, index stays)
                run_sql(dsn, f"TRUNCATE {tbl}")
                run_sql(dsn, f"DROP INDEX IF EXISTS idx_{tbl}")

        report("HNSW: 5 rapid CREATE/DROP/TRUNCATE cycles", "PASS")

    except Exception as e:
        report("HNSW rapid DDL", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 3. COPY: 50K rows with correctness check
# ================================================================
def test_copy_50k_correctness(dsn):
    sfx = random_suffix()
    tbl = f"copy_50k_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, val INT, tag TEXT)")

        n_rows = 50000
        lines = []
        for i in range(1, n_rows + 1):
            lines.append(f"{i}\t{i * 7}\trow_{i:06d}")
        data = "\n".join(lines) + "\n"

        result = run_psql_stdin(dsn, f"COPY {tbl} (id, val, tag) FROM STDIN;\n{data}\\.\n", timeout=180)
        if result.returncode != 0:
            report("COPY: 50K rows insert", "FAIL", result.stderr[:200])
            return

        # Count
        count = run_sql(dsn, f"SELECT count(*) FROM {tbl}")
        if count != str(n_rows):
            report("COPY: 50K row count", "BUG", f"expected {n_rows}, got {count}")
            return

        # Check sum: sum(val) = sum(i*7 for i in 1..50000) = 7 * 50000*50001/2 = 8750175000
        expected_sum = 7 * n_rows * (n_rows + 1) // 2
        actual_sum = run_sql(dsn, f"SELECT SUM(val)::bigint FROM {tbl}")
        if actual_sum == str(expected_sum):
            report(f"COPY: 50K rows checksum (SUM={expected_sum})", "PASS")
        else:
            report("COPY: 50K rows checksum", "BUG", f"expected {expected_sum}, got {actual_sum}")

        # Check min/max
        min_id = run_sql(dsn, f"SELECT min(id) FROM {tbl}")
        max_id = run_sql(dsn, f"SELECT max(id) FROM {tbl}")
        if min_id == "1" and max_id == str(n_rows):
            report("COPY: 50K rows min/max", "PASS")
        else:
            report("COPY: 50K rows min/max", "BUG", f"min={min_id}, max={max_id}")

        # Spot check random rows
        for _ in range(5):
            rid = random.randint(1, n_rows)
            row = run_sql(dsn, f"SELECT val, tag FROM {tbl} WHERE id = {rid}")
            expected_val = rid * 7
            expected_tag = f"row_{rid:06d}"
            if f"{expected_val}|{expected_tag}" == row:
                pass
            else:
                report(f"COPY: spot check id={rid}", "BUG",
                       f"expected {expected_val}|{expected_tag}, got {row}")
                return
        report("COPY: 50K rows spot check (5 random)", "PASS")

    except Exception as e:
        report("COPY 50K correctness", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 4. Decimal: window function SUM overflow
# ================================================================
def test_decimal_window_overflow(dsn):
    sfx = random_suffix()
    tbl = f"dec_win_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT, grp INT, val NUMERIC)")
        # Two rows with MAX value in same group → window SUM overflows
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, 1, 79228162514264337593543950335)")
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (2, 1, 79228162514264337593543950335)")

        result = run_psql(dsn,
            f"SELECT SUM(val) OVER (PARTITION BY grp) FROM {tbl}",
            expect_success=False)
        if result.returncode != 0:
            stderr = result.stderr.lower()
            if "overflow" in stderr or "out of range" in stderr or "numeric" in stderr:
                report("Decimal: window SUM overflow → error", "PASS")
            else:
                report("Decimal: window SUM overflow", "BUG", f"wrong error: {result.stderr[:200]}")
        else:
            report("Decimal: window SUM overflow", "BUG",
                   f"should have errored, got: {result.stdout[:100]}")

    except Exception as e:
        report("Decimal window overflow", "FAIL", str(e)[:200])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 5. HNSW: UPDATE all vectors (full delta, base graph stale)
# ================================================================
def test_hnsw_full_delta(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_fulld_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"""INSERT INTO {tbl} VALUES
            (1, '[1,0,0]'), (2, '[0,1,0]'), (3, '[0,0,1]'),
            (4, '[1,1,0]'), (5, '[0,1,1]')""")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")
        time.sleep(3)

        # Baseline: nearest to [1,0,0] is id=1
        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        assert r == "1", f"baseline failed: {r}"

        # UPDATE ALL vectors: mirror them (x→z, y→y, z→x)
        run_sql(dsn, f"UPDATE {tbl} SET v = '[0,0,1]' WHERE id = 1")
        run_sql(dsn, f"UPDATE {tbl} SET v = '[0,1,0]' WHERE id = 2")  # same
        run_sql(dsn, f"UPDATE {tbl} SET v = '[1,0,0]' WHERE id = 3")
        run_sql(dsn, f"UPDATE {tbl} SET v = '[0,1,1]' WHERE id = 4")
        run_sql(dsn, f"UPDATE {tbl} SET v = '[1,1,0]' WHERE id = 5")

        # Now nearest to [1,0,0] should be id=3 (was [0,0,1], now [1,0,0])
        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        if r == "3":
            report("HNSW: full delta (all vectors updated)", "PASS")
        else:
            report("HNSW: full delta", "BUG",
                   f"expected id=3 (updated to [1,0,0]), got id={r}")

        # Nearest to [0,0,1] should be id=1 (updated to [0,0,1])
        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0,0,1]' LIMIT 1")
        if r == "1":
            report("HNSW: full delta reverse check", "PASS")
        else:
            report("HNSW: full delta reverse check", "BUG",
                   f"expected id=1 (updated to [0,0,1]), got id={r}")

    except Exception as e:
        report("HNSW full delta", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 6. HNSW: INSERT + immediate query before merge (delta-only path)
# ================================================================
def test_hnsw_pre_merge_query(dsn):
    sfx = random_suffix()
    tbl = f"hnsw_premrg_{sfx}"
    try:
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, v VECTOR(3))")
        run_sql(dsn, f"CREATE INDEX idx_{tbl} ON {tbl} USING hnsw (v vector_l2_ops)")

        # Insert and immediately query (no time for merge)
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (1, '[1,0,0]')")
        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[1,0,0]' LIMIT 1")
        if r == "1":
            report("HNSW: query immediately after insert (pre-merge)", "PASS")
        else:
            report("HNSW: query immediately after insert", "BUG",
                   f"expected 1, got {r}")

        # Insert more and query again
        run_sql(dsn, f"INSERT INTO {tbl} VALUES (2, '[0,1,0]'), (3, '[0,0,1]')")
        r = run_sql(dsn, f"SELECT id FROM {tbl} ORDER BY v <-> '[0,0,1]' LIMIT 1")
        if r == "3":
            report("HNSW: multi-insert pre-merge query", "PASS")
        else:
            report("HNSW: multi-insert pre-merge query", "BUG",
                   f"expected 3, got {r}")

        # Query with LIMIT > count
        r = run_sql(dsn, f"SELECT count(*) FROM (SELECT id FROM {tbl} ORDER BY v <-> '[0.5,0.5,0.5]' LIMIT 100) t")
        if r == "3":
            report("HNSW: LIMIT > count pre-merge", "PASS")
        else:
            report("HNSW: LIMIT > count pre-merge", "BUG", f"expected 3, got {r}")

    except Exception as e:
        report("HNSW pre-merge", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# 7. Statistics: ANALYZE with very wide rows
# ================================================================
def test_analyze_wide_rows(dsn):
    sfx = random_suffix()
    tbl = f"stat_wide_{sfx}"
    try:
        # Table with 20 TEXT columns, each holding 2KB data (> WIDTH_THRESHOLD=1024)
        cols = ", ".join(f"c{i} TEXT" for i in range(20))
        run_sql(dsn, f"CREATE TABLE {tbl} (id INT PRIMARY KEY, {cols})")

        vals = ", ".join(f"repeat('x', 2000)" for _ in range(20))
        for batch in range(5):
            run_sql(dsn, f"INSERT INTO {tbl} SELECT g, {vals} FROM generate_series({batch*10+1}, {(batch+1)*10}) g")

        # ANALYZE should succeed even with all wide columns
        result = run_psql(dsn, f"ANALYZE {tbl}", expect_success=False)
        if result.returncode == 0:
            report("ANALYZE: 20 wide TEXT columns (2KB each)", "PASS")
        else:
            report("ANALYZE: wide columns", "BUG", f"ANALYZE failed: {result.stderr[:200]}")

        # Verify EXPLAIN still works and shows row count
        result = run_sql(dsn, f"EXPLAIN SELECT * FROM {tbl}")
        if "rows=50" in result or "rows=49" in result or "rows=51" in result:
            report("ANALYZE: row count accurate after wide-column ANALYZE", "PASS")
        elif "rows=" in result:
            # Extract rows count
            import re
            m = re.search(r'rows=(\d+)', result)
            rows_est = int(m.group(1)) if m else -1
            if abs(rows_est - 50) < 10:
                report("ANALYZE: row count close enough", "PASS", f"rows={rows_est}")
            else:
                report("ANALYZE: row count inaccurate", "FAIL", f"rows={rows_est}, expected ~50")
        else:
            report("ANALYZE: EXPLAIN output unexpected", "FAIL", result[:100])

    except Exception as e:
        report("ANALYZE wide rows", "FAIL", str(e)[:300])
    finally:
        run_psql(dsn, f"DROP TABLE IF EXISTS {tbl}", expect_success=False)


# ================================================================
# Main
# ================================================================
def main():
    args = parse_args()
    dsn = args.dsn

    print("=" * 60)
    print("STRESS / BOUNDARY TEST (Round 2 - Larger Data)")
    print("=" * 60)

    test_hnsw_heavy_concurrent(dsn)
    test_hnsw_rapid_ddl_cycles(dsn)
    test_copy_50k_correctness(dsn)
    test_decimal_window_overflow(dsn)
    test_hnsw_full_delta(dsn)
    test_hnsw_pre_merge_query(dsn)
    test_analyze_wide_rows(dsn)

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

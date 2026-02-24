#!/usr/bin/env python3
"""
Extended protocol smoke test for db9-server.

Exercises do_describe_statement by sending parameterized queries through the
PostgreSQL extended query protocol (Parse/Bind/Describe/Execute). Uses psycopg3
which sends server-side parameters via the wire protocol, unlike psycopg2 which
does client-side mogrification.

Each query hits a different describe code path. Designed as a fast (<10s)
fail-fast gate.

Usage:
    python3 scripts/extended_protocol_smoke.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres
"""

import argparse
import os
import sys
import time


def main():
    parser = argparse.ArgumentParser(description="Extended protocol smoke test")
    default_dsn = os.environ.get(
        "PG_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
    )
    parser.add_argument("--dsn", default=default_dsn, help="PostgreSQL DSN")
    args = parser.parse_args()

    try:
        import psycopg
    except ImportError:
        print(
            "SKIP: psycopg (v3) not installed (pip install psycopg)",
            file=sys.stderr,
        )
        sys.exit(0)

    conn = psycopg.connect(args.dsn, autocommit=True)
    cur = conn.cursor()

    # Setup: create test tables (DDL via simple query — no params)
    cur.execute("DROP TABLE IF EXISTS _ext_smoke_t2")
    cur.execute("DROP TABLE IF EXISTS _ext_smoke_t1")
    cur.execute(
        "CREATE TABLE _ext_smoke_t1 (id INT PRIMARY KEY, col TEXT, val INT)"
    )
    cur.execute(
        "CREATE TABLE _ext_smoke_t2 (id INT PRIMARY KEY, fk INT REFERENCES _ext_smoke_t1(id), note TEXT)"
    )
    cur.execute("INSERT INTO _ext_smoke_t1 VALUES (1, 'a', 10), (2, 'b', 20), (3, 'a', 30)")
    cur.execute("INSERT INTO _ext_smoke_t2 VALUES (1, 1, 'x'), (2, 2, 'y')")

    # psycopg3 uses %s in Python but sends $1 on the wire via extended protocol.
    # Each test: (name, query_with_percent_s_placeholders, params)
    tests = [
        (
            "simple_cast",
            "SELECT %s::int",
            (42,),
        ),
        (
            "multi_param_types",
            "SELECT %s::text, %s::int, %s::bool",
            ("hello", 7, True),
        ),
        (
            "insert_returning",
            "INSERT INTO _ext_smoke_t1 (id, col, val) VALUES (%s, %s, %s) RETURNING *",
            (100, "test", 999),
        ),
        (
            "select_where_param",
            "SELECT * FROM _ext_smoke_t1 WHERE id = %s",
            (1,),
        ),
        (
            "update_returning",
            "UPDATE _ext_smoke_t1 SET col = %s WHERE id = %s RETURNING *",
            ("updated", 100),
        ),
        (
            "delete_returning",
            "DELETE FROM _ext_smoke_t1 WHERE id = %s RETURNING *",
            (100,),
        ),
        (
            "join_with_param",
            "SELECT * FROM _ext_smoke_t1 t1 JOIN _ext_smoke_t2 t2 ON t1.id = t2.fk WHERE t1.col = %s",
            ("a",),
        ),
        (
            "subquery_describe",
            "SELECT * FROM (SELECT %s::int AS v) sub",
            (5,),
        ),
        (
            "aggregate_having_param",
            "SELECT col, count(*) FROM _ext_smoke_t1 GROUP BY col HAVING count(*) > %s",
            (0,),
        ),
        (
            "cte_with_param",
            "WITH cte AS (SELECT * FROM _ext_smoke_t1 WHERE id = %s) SELECT * FROM cte",
            (1,),
        ),
        (
            "in_list_params",
            "SELECT * FROM _ext_smoke_t1 WHERE id IN (%s, %s, %s)",
            (1, 2, 3),
        ),
        (
            "nested_subquery_3_levels",
            "SELECT * FROM (SELECT * FROM (SELECT %s::int AS v) s1) s2",
            (42,),
        ),
    ]

    # Build a deep nesting stress test (regression for #819/#820 stack overflow).
    # 20 levels is enough to trigger the original bug without being too slow.
    deep_sql = "SELECT %s::int AS v"
    for i in range(1, 21):
        deep_sql = f"SELECT * FROM ({deep_sql}) t{i}"
    tests.append(("deep_nested_subquery_20_levels", deep_sql, (1,)))

    # Regression tests for #907: prepared scalar expression and catalog-heavy
    # subquery must not stack-overflow under default 8 MiB worker stack.
    # Run with DB9_TOKIO_STACK_MB=8 to get a deterministic signal.
    # 4th element is an optional (checker_fn, description) for result validation.
    tests.append(
        (
            "issue_907_prepared_scalar_expr",
            "SELECT %s::int + 1",
            (42,),
            (lambda rows: rows == [(43,)], "expected [(43,)]"),
        )
    )
    tests.append(
        (
            "issue_907_catalog_subquery",
            "SELECT (SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = %s)",
            ("public",),
            (
                lambda rows: len(rows) == 1 and len(rows[0]) == 1 and isinstance(rows[0][0], int) and rows[0][0] >= 0,
                "expected 1 row with non-negative integer count",
            ),
        )
    )

    passed = 0
    failed = 0
    total = len(tests)
    start = time.monotonic()

    for test_entry in tests:
        name, query, params = test_entry[0], test_entry[1], test_entry[2]
        checker = test_entry[3] if len(test_entry) > 3 else None
        test_start = time.monotonic()
        try:
            cur.execute(query, params)
            rows = cur.fetchall()
            if checker is not None:
                check_fn, check_desc = checker
                if not check_fn(rows):
                    raise AssertionError(
                        f"result mismatch: got {rows!r}, {check_desc}"
                    )
            elapsed = time.monotonic() - test_start
            if elapsed > 5.0:
                print(f"WARN [{name}] slow: {elapsed:.1f}s")
            print(f"PASS [{name}] ({elapsed:.2f}s)")
            passed += 1
        except Exception as e:
            elapsed = time.monotonic() - test_start
            print(f"FAIL [{name}] ({elapsed:.2f}s): {e}")
            failed += 1

    # Cleanup
    try:
        cur.execute("DROP TABLE IF EXISTS _ext_smoke_t2")
        cur.execute("DROP TABLE IF EXISTS _ext_smoke_t1")
    except Exception:
        pass

    cur.close()
    conn.close()

    total_time = time.monotonic() - start
    print(f"\n{'=' * 50}")
    print(f"Extended protocol smoke: {passed}/{total} passed ({total_time:.1f}s)")

    if failed > 0:
        print(f"FAILED: {failed} test(s)")
        sys.exit(1)
    else:
        print("All extended protocol smoke tests passed.")
        sys.exit(0)


if __name__ == "__main__":
    main()

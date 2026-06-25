#!/usr/bin/env python3
"""
Transaction mode correctness.

READ ONLY must actually reject database writes, including COPY FROM and
SELECT FOR UPDATE. Isolation-level readback reports db9's effective TiKV
snapshot-isolation behavior.
"""

import argparse
import os
import random
import sys
import time

try:
    import psycopg
except ImportError:
    print("ERROR: psycopg is required; run with `uv run --with 'psycopg[binary]'`", file=sys.stderr)
    raise


def expect_sqlstate(fn, sqlstate, label):
    try:
        fn()
    except psycopg.Error as exc:
        if exc.sqlstate == sqlstate:
            return
        print(f"FAIL: {label} failed with SQLSTATE {exc.sqlstate}, expected {sqlstate}")
        print(exc)
        sys.exit(1)
    print(f"FAIL: {label} succeeded; expected SQLSTATE {sqlstate}")
    sys.exit(1)


def exec_one(conn, sql):
    with conn.cursor() as cur:
        cur.execute(sql)
        row = cur.fetchone()
        return row[0] if row else None


def cleanup_rollback(conn):
    try:
        conn.rollback()
    except Exception:
        pass


def main():
    parser = argparse.ArgumentParser(description="transaction mode correctness")
    parser.add_argument(
        "--dsn",
        default=os.environ.get(
            "TEST_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
        ),
    )
    args = parser.parse_args()

    table = f"txn_modes_{int(time.time())}_{random.randint(1000, 9999)}"
    with psycopg.connect(args.dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute(f"DROP TABLE IF EXISTS {table} CASCADE")
            cur.execute(f"CREATE TABLE {table}(id int primary key)")
            cur.execute(f"INSERT INTO {table} VALUES (1)")

    def readonly_insert():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute(f"INSERT INTO {table} VALUES (2)")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(readonly_insert, "25006", "BEGIN READ ONLY INSERT")

    def nested_begin_read_write_does_not_revoke_readonly():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute("BEGIN READ WRITE")
                    cur.execute(f"INSERT INTO {table} VALUES (20)")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(
        nested_begin_read_write_does_not_revoke_readonly,
        "25006",
        "nested BEGIN READ WRITE inside READ ONLY transaction",
    )

    def default_readonly_insert():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("SET default_transaction_read_only = on")
                cur.execute(f"INSERT INTO {table} VALUES (3)")

    expect_sqlstate(default_readonly_insert, "25006", "default read-only INSERT")

    def readonly_select_for_update():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute(f"SELECT * FROM {table} FOR UPDATE")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(readonly_select_for_update, "25006", "READ ONLY SELECT FOR UPDATE")

    def readonly_copy():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    with cur.copy(f"COPY {table}(id) FROM STDIN") as copy:
                        copy.write_row((4,))
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(readonly_copy, "25006", "READ ONLY COPY FROM")

    def late_set_transaction():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN")
                try:
                    cur.execute("SELECT 1")
                    cur.execute("SET TRANSACTION READ ONLY")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(late_set_transaction, "25001", "late SET TRANSACTION")

    def late_set_transaction_read_only_guc():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute("SELECT 1")
                    cur.execute("SET LOCAL transaction_read_only = off")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(
        late_set_transaction_read_only_guc,
        "25001",
        "late SET LOCAL transaction_read_only",
    )

    def late_set_config_transaction_read_only():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute("SELECT 1")
                    cur.execute("SELECT set_config('transaction_read_only', 'off', true)")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(
        late_set_config_transaction_read_only,
        "25001",
        "late set_config transaction_read_only",
    )

    def late_reset_transaction_read_only():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute("SELECT 1")
                    cur.execute("RESET transaction_read_only")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(
        late_reset_transaction_read_only,
        "25001",
        "late RESET transaction_read_only",
    )

    def late_reset_all():
        with psycopg.connect(args.dsn, autocommit=True) as conn:
            with conn.cursor() as cur:
                cur.execute("BEGIN READ ONLY")
                try:
                    cur.execute("SELECT 1")
                    cur.execute("RESET ALL")
                finally:
                    cleanup_rollback(conn)

    expect_sqlstate(late_reset_all, "25001", "late RESET ALL")

    with psycopg.connect(args.dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
            if exec_one(conn, "SHOW default_transaction_read_only") != "on":
                print("FAIL: default_transaction_read_only did not become on")
                sys.exit(1)
            cur.execute("BEGIN")
            try:
                if exec_one(conn, "SHOW transaction_read_only") != "on":
                    print("FAIL: transaction_read_only did not inherit default on")
                    sys.exit(1)
            finally:
                cleanup_rollback(conn)
            cur.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE")
            if exec_one(conn, "SHOW default_transaction_read_only") != "off":
                print("FAIL: default_transaction_read_only did not become off")
                sys.exit(1)

    with psycopg.connect(args.dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            if exec_one(conn, "SHOW transaction_isolation") != "repeatable read":
                print("FAIL: transaction_isolation did not report effective repeatable read")
                sys.exit(1)

    with psycopg.connect(args.dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute("BEGIN")
            try:
                cur.execute("SET LOCAL transaction_isolation = 'serializable'")
                if exec_one(conn, "SHOW transaction_isolation") != "repeatable read":
                    print("FAIL: local transaction_isolation override leaked into readback")
                    sys.exit(1)
            finally:
                cleanup_rollback(conn)

    with psycopg.connect(args.dsn, autocommit=True) as conn:
        count = exec_one(conn, f"SELECT count(*) FROM {table}")
        min_id = exec_one(conn, f"SELECT min(id) FROM {table}")
        max_id = exec_one(conn, f"SELECT max(id) FROM {table}")
        with conn.cursor() as cur:
            cur.execute(f"DROP TABLE {table}")
    if (count, min_id, max_id) != (1, 1, 1):
        print(f"FAIL: read-only writes changed table: count={count}, min={min_id}, max={max_id}")
        sys.exit(1)

    print("PASS")


if __name__ == "__main__":
    main()

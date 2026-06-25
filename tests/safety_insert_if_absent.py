#!/usr/bin/env python3
"""
Concurrent INSERT/COPY insert-if-absent guard.

Txn A inserts a primary key and keeps the transaction open. Txn B then tries to
write the same key before A commits. B must not silently overwrite A after the
lock clears; exactly one row with A's value should survive.
"""

import argparse
import os
import random
import sys
import threading
import time

from safety_pg import exec_all_text, exec_sql, psycopg, sql_error


def assert_insert_if_absent(dsn, table, contender_sql, label):
    exec_sql(dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    exec_sql(dsn, f"CREATE TABLE {table}(id int primary key, v text not null)")

    txn_a = {"error": None}
    txn_a_insert_ready = threading.Event()

    def tx_a():
        def run():
            with psycopg.connect(dsn, autocommit=True) as conn:
                with conn.cursor() as cur:
                    cur.execute("BEGIN")
                    cur.execute(f"INSERT INTO {table} VALUES (1, 'winner')")
                    txn_a_insert_ready.set()
                    cur.execute("SELECT pg_sleep(1)")
                    cur.execute("COMMIT")

        txn_a["error"] = sql_error(run)

    thread = threading.Thread(target=tx_a)
    thread.start()
    if not txn_a_insert_ready.wait(timeout=10):
        print(f"FAIL: {label}: transaction A did not reach duplicate-key insert")
        sys.exit(1)
    contender_error = sql_error(lambda: contender_sql(dsn, table))
    thread.join(timeout=30)

    if thread.is_alive():
        raise RuntimeError(f"{label}: transaction A did not finish")
    if txn_a["error"] is not None:
        print(f"FAIL: {label}: transaction A failed")
        print(txn_a["error"])
        sys.exit(1)
    if contender_error is None:
        print(f"FAIL: {label}: concurrent duplicate write succeeded")
        sys.exit(1)

    final = exec_all_text(
        dsn,
        f"SELECT count(*), min(v), max(v) FROM {table} WHERE id = 1",
    )
    exec_sql(dsn, f"DROP TABLE {table}")
    if final != "1|winner|winner":
        print(f"FAIL: {label}: expected one surviving winner row, got {final}")
        print(contender_error)
        sys.exit(1)


def assert_update_pk_insert_if_absent(dsn, table):
    exec_sql(dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    exec_sql(dsn, f"CREATE TABLE {table}(id int primary key, v text not null)")
    exec_sql(dsn, f"INSERT INTO {table} VALUES (1, 'mover')")

    txn_a = {"error": None}
    txn_a_insert_ready = threading.Event()

    def tx_a():
        def run():
            with psycopg.connect(dsn, autocommit=True) as conn:
                with conn.cursor() as cur:
                    cur.execute("BEGIN")
                    cur.execute(f"INSERT INTO {table} VALUES (2, 'winner')")
                    txn_a_insert_ready.set()
                    cur.execute("SELECT pg_sleep(1)")
                    cur.execute("COMMIT")

        txn_a["error"] = sql_error(run)

    thread = threading.Thread(target=tx_a)
    thread.start()
    if not txn_a_insert_ready.wait(timeout=10):
        print("FAIL: UPDATE PK: transaction A did not reach duplicate-key insert")
        sys.exit(1)
    contender_error = sql_error(
        lambda: exec_sql(dsn, f"UPDATE {table} SET id = 2, v = 'loser' WHERE id = 1")
    )
    thread.join(timeout=30)

    if thread.is_alive():
        raise RuntimeError("UPDATE PK: transaction A did not finish")
    if txn_a["error"] is not None:
        print("FAIL: UPDATE PK: transaction A failed")
        print(txn_a["error"])
        sys.exit(1)
    if contender_error is None:
        print("FAIL: UPDATE PK: concurrent move into duplicate key succeeded")
        sys.exit(1)

    final = exec_all_text(
        dsn,
        f"SELECT id, v FROM {table} ORDER BY id",
    )
    exec_sql(dsn, f"DROP TABLE {table}")
    if final != "1|mover\n2|winner":
        print(f"FAIL: UPDATE PK: expected original and winner rows, got {final}")
        print(contender_error)
        sys.exit(1)


def assert_update_pk_recreates_unique_index(dsn, table):
    exec_sql(dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    exec_sql(dsn, f"CREATE TABLE {table}(id int primary key, email text unique)")
    exec_sql(dsn, f"INSERT INTO {table} VALUES (1, 'a@example.com')")
    exec_sql(dsn, f"UPDATE {table} SET id = 2 WHERE id = 1")
    final = exec_all_text(dsn, f"SELECT id, email FROM {table}")
    exec_sql(dsn, f"DROP TABLE {table}")
    if final != "2|a@example.com":
        print(f"FAIL: UPDATE PK unique-index recreation returned {final}")
        sys.exit(1)


def assert_batch_update_pk_shift_preserves_rows(dsn, table):
    exec_sql(dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    exec_sql(dsn, f"CREATE TABLE {table}(id int primary key, v text not null)")
    exec_sql(dsn, f"INSERT INTO {table} VALUES (1, 'a'), (2, 'b')")
    exec_sql(dsn, f"UPDATE {table} SET id = id - 1")
    final = exec_all_text(dsn, f"SELECT id, v FROM {table} ORDER BY id")
    exec_sql(dsn, f"DROP TABLE {table}")
    if final != "0|a\n1|b":
        print(f"FAIL: batch UPDATE PK shift lost rows or order, got {final}")
        sys.exit(1)


def insert_duplicate(dsn, table):
    exec_sql(dsn, f"INSERT INTO {table} VALUES (1, 'loser')")


def copy_duplicate(dsn, table):
    with psycopg.connect(dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            with cur.copy(f"COPY {table}(id, v) FROM STDIN") as copy:
                copy.write_row((1, "loser"))


def main():
    parser = argparse.ArgumentParser(description="Concurrent insert-if-absent guard")
    parser.add_argument(
        "--dsn",
        default=os.environ.get(
            "TEST_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
        ),
    )
    args = parser.parse_args()

    suffix = f"{int(time.time())}_{random.randint(1000, 9999)}"
    assert_insert_if_absent(
        args.dsn,
        f"insert_absent_insert_{suffix}",
        insert_duplicate,
        "INSERT",
    )
    assert_insert_if_absent(
        args.dsn,
        f"insert_absent_copy_{suffix}",
        copy_duplicate,
        "COPY",
    )
    assert_update_pk_insert_if_absent(args.dsn, f"insert_absent_update_pk_{suffix}")
    assert_update_pk_recreates_unique_index(
        args.dsn, f"insert_absent_update_unique_{suffix}"
    )
    assert_batch_update_pk_shift_preserves_rows(
        args.dsn, f"insert_absent_update_shift_{suffix}"
    )

    print("PASS")


if __name__ == "__main__":
    main()

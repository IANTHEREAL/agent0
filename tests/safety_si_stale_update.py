#!/usr/bin/env python3
"""
Snapshot-isolation stale UPDATE guard.

Txn A starts and reads a row. Txn B commits a newer version while A is still
open. When A later tries to UPDATE that snapshot row, db9 must return 40001
instead of recomputing from the newer value and silently overwriting it.
"""

import argparse
import os
import random
import sys
import threading
import time

from safety_pg import exec_one, exec_sql, psycopg, sql_error


def main():
    parser = argparse.ArgumentParser(description="SI stale UPDATE guard")
    parser.add_argument(
        "--dsn",
        default=os.environ.get(
            "TEST_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
        ),
    )
    args = parser.parse_args()

    table = f"si_stale_update_{int(time.time())}_{random.randint(1000, 9999)}"
    exec_sql(args.dsn, f"DROP TABLE IF EXISTS {table} CASCADE")
    exec_sql(args.dsn, f"CREATE TABLE {table}(id int primary key, v int not null)")
    exec_sql(args.dsn, f"INSERT INTO {table} VALUES (1, 0)")

    txn_a = {"error": None}
    txn_a_snapshot_ready = threading.Event()

    def tx_a():
        def run():
            with psycopg.connect(args.dsn, autocommit=True) as conn:
                with conn.cursor() as cur:
                    cur.execute("BEGIN")
                    cur.execute(f"SELECT v FROM {table} WHERE id = 1")
                    row = cur.fetchone()
                    if row is None or row[0] != 0:
                        raise RuntimeError(f"unexpected snapshot row: {row}")
                    txn_a_snapshot_ready.set()
                    cur.execute("SELECT pg_sleep(1)")
                    cur.execute(f"UPDATE {table} SET v = v + 1 WHERE id = 1")
                    cur.execute("COMMIT")

        txn_a["error"] = sql_error(run)

    thread = threading.Thread(target=tx_a)
    thread.start()
    if not txn_a_snapshot_ready.wait(timeout=10):
        print("FAIL: transaction A did not reach snapshot read")
        sys.exit(1)
    exec_sql(args.dsn, f"UPDATE {table} SET v = 10 WHERE id = 1")
    thread.join(timeout=30)
    if thread.is_alive():
        print("FAIL: transaction A did not finish")
        sys.exit(1)

    err = txn_a["error"]
    if err is None:
        print("FAIL: stale UPDATE succeeded; expected SQLSTATE 40001")
        sys.exit(1)
    if err.sqlstate != "40001":
        print(f"FAIL: stale UPDATE failed with SQLSTATE {err.sqlstate}, expected 40001")
        print(err)
        sys.exit(1)

    final = exec_one(args.dsn, f"SELECT v FROM {table} WHERE id = 1")
    exec_sql(args.dsn, f"DROP TABLE {table}")
    if final != 10:
        print(f"FAIL: expected Txn B value 10 to survive, got {final}")
        sys.exit(1)

    print("PASS")


if __name__ == "__main__":
    main()

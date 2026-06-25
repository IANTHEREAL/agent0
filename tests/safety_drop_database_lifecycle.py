#!/usr/bin/env python3
"""
DROP DATABASE lifecycle guard.

DB9 currently follows PostgreSQL's KISS behavior for SQL DROP DATABASE: do not
drop a database while another session is connected to it. Once the connected
session exits, DROP succeeds and new connections to that database fail.
"""

import argparse
import os
import random
import sys
import threading
import time
from urllib.parse import urlsplit, urlunsplit

from safety_pg import exec_sql, psycopg, sql_error


def db_dsn(base_dsn, database):
    parts = urlsplit(base_dsn)
    return urlunsplit((parts.scheme, parts.netloc, f"/{database}", parts.query, parts.fragment))


def exec_on_dsn(dsn, sql):
    with psycopg.connect(dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute(sql)


def main():
    parser = argparse.ArgumentParser(description="DROP DATABASE lifecycle guard")
    parser.add_argument(
        "--dsn",
        default=os.environ.get(
            "TEST_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
        ),
    )
    args = parser.parse_args()

    db_name = f"drop_lifecycle_{int(time.time())}_{random.randint(1000, 9999)}"
    exec_sql(args.dsn, f"DROP DATABASE IF EXISTS {db_name}")
    exec_sql(args.dsn, f"CREATE DATABASE {db_name}")

    holder = {"error": None}

    def hold_connection():
        holder["error"] = sql_error(
            lambda: exec_on_dsn(db_dsn(args.dsn, db_name), "SELECT pg_sleep(2)")
        )

    thread = threading.Thread(target=hold_connection)
    thread.start()
    time.sleep(0.4)

    blocked_drop = sql_error(lambda: exec_sql(args.dsn, f"DROP DATABASE {db_name}"))
    if blocked_drop is None:
        print("FAIL: DROP DATABASE succeeded while another session was connected")
        sys.exit(1)
    if blocked_drop.sqlstate != "55006":
        print(
            f"FAIL: DROP DATABASE failed with SQLSTATE {blocked_drop.sqlstate}, expected 55006"
        )
        print(blocked_drop)
        sys.exit(1)

    thread.join(timeout=10)
    if thread.is_alive():
        print("FAIL: holder connection did not finish")
        sys.exit(1)
    if holder["error"] is not None:
        print("FAIL: holder connection failed unexpectedly")
        print(holder["error"])
        sys.exit(1)

    exec_sql(args.dsn, f"DROP DATABASE {db_name}")
    reconnect = sql_error(lambda: exec_on_dsn(db_dsn(args.dsn, db_name), "SELECT 1"))
    if reconnect is None:
        print("FAIL: connected to dropped database")
        sys.exit(1)
    if reconnect.sqlstate != "3D000" and "does not exist" not in str(reconnect):
        print(
            f"FAIL: reconnect to dropped database failed with SQLSTATE {reconnect.sqlstate}, expected 3D000"
        )
        print(reconnect)
        sys.exit(1)

    print("PASS")


if __name__ == "__main__":
    main()

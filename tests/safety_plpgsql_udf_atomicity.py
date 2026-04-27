#!/usr/bin/env python3
"""
Concurrent PL/pgSQL UDF atomicity guard.

Reproduces the paired #2402/#4921 correctness blocker without HammerDB or
TPC-C helper SQL. The minimal function only updates a single counter row and
returns 1. Under correct transactional semantics, 25 successful calls with
delta=5 must end with counter=125.

Current bug signature this test is meant to catch:
  - every client call returns success (`1`)
  - but the final counter is lower than expected (silent lost update)
"""

import argparse
import os
import sys
import threading
import uuid
from typing import Optional

import subprocess
from urllib.parse import urlparse


def parse_dsn(dsn: str):
    parsed = urlparse(dsn)
    if parsed.scheme not in ("postgres", "postgresql"):
        raise ValueError(f"unsupported dsn scheme: {parsed.scheme}")
    return {
        "host": parsed.hostname or "127.0.0.1",
        "port": str(parsed.port or 5433),
        "user": parsed.username or "admin",
        "password": parsed.password or "",
        "dbname": parsed.path.lstrip("/") or "postgres",
    }


def run_psql(conninfo, sql: str, dbname: Optional[str] = None, check: bool = True) -> str:
    env = os.environ.copy()
    env["PGPASSWORD"] = conninfo["password"]
    cmd = [
        "psql",
        "-X",
        "-h",
        conninfo["host"],
        "-p",
        conninfo["port"],
        "-U",
        conninfo["user"],
        "-d",
        dbname or conninfo["dbname"],
        "-v",
        "ON_ERROR_STOP=1",
        "-Atq",
        "-c",
        sql,
    ]
    proc = subprocess.run(cmd, env=env, capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip())
    return proc.stdout.strip()


def exec_batch(conninfo, sql: str, dbname: Optional[str] = None):
    env = os.environ.copy()
    env["PGPASSWORD"] = conninfo["password"]
    cmd = [
        "psql",
        "-X",
        "-h",
        conninfo["host"],
        "-p",
        conninfo["port"],
        "-U",
        conninfo["user"],
        "-d",
        dbname or conninfo["dbname"],
    ]
    proc = subprocess.run(cmd, env=env, input=sql, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip())


def main():
    parser = argparse.ArgumentParser(
        description="Concurrent PL/pgSQL UDF atomicity regression guard"
    )
    parser.add_argument(
        "--dsn",
        default=os.environ.get(
            "TEST_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres"
        ),
        help="PostgreSQL DSN for db9-server",
    )
    parser.add_argument("--threads", type=int, default=5)
    parser.add_argument("--iters", type=int, default=5)
    parser.add_argument("--delta", type=int, default=5)
    args = parser.parse_args()

    conninfo = parse_dsn(args.dsn)
    schema = f"udf_atomicity_{uuid.uuid4().hex[:8]}"
    expected_calls = args.threads * args.iters
    expected_total = expected_calls * args.delta

    run_psql(conninfo, f"CREATE SCHEMA {schema}")
    run_psql(conninfo, f"CREATE TABLE {schema}.acc(id int primary key, v int not null)")
    run_psql(conninfo, f"INSERT INTO {schema}.acc VALUES (1,0)")
    run_psql(
        conninfo,
        f"""
CREATE OR REPLACE FUNCTION {schema}.bump_only(delta int) RETURNS int AS $$
BEGIN
  UPDATE {schema}.acc
  SET v = v + delta
  WHERE id = 1;
  RETURN 1;
END;
$$ LANGUAGE plpgsql
""",
    )

    results = []
    errors = []
    lock = threading.Lock()

    def worker():
        try:
            for _ in range(args.iters):
                out = run_psql(conninfo, f"SELECT {schema}.bump_only({args.delta})")
                with lock:
                    results.append(out)
        except Exception as e:
            with lock:
                errors.append(str(e))

    threads = [threading.Thread(target=worker) for _ in range(args.threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    final_v = int(run_psql(conninfo, f"SELECT v FROM {schema}.acc WHERE id = 1"))

    print(f"threads={args.threads} iters={args.iters} delta={args.delta}")
    print(f"calls={len(results)} expected_calls={expected_calls}")
    print(f"all_returns_one={all(v == '1' for v in results)}")
    print(f"errors={len(errors)}")
    print(f"final_counter={final_v} expected_counter={expected_total}")

    ok = True
    if errors:
        ok = False
        for err in errors[:5]:
            print(f"ERROR: {err}")
    if len(results) != expected_calls:
        ok = False
        print("FAIL: not every call returned a result")
    if not all(v == "1" for v in results):
        ok = False
        print("FAIL: at least one call did not return 1")
    if final_v != expected_total:
        ok = False
        print("FAIL: final counter does not match successful calls")

    run_psql(conninfo, f"DROP SCHEMA {schema} CASCADE")

    if not ok:
        sys.exit(1)

    print("PASS")


if __name__ == "__main__":
    main()

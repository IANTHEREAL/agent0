#!/usr/bin/env python3
import argparse
import os
import subprocess


def run_psql(host: str, port: int, user: str, password: str, sql: str) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["PGPASSWORD"] = password
    return subprocess.run(
        [
            "psql",
            "-X",
            "-qAt",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            host,
            "-p",
            str(port),
            "-U",
            user,
            "-d",
            "postgres",
            "-c",
            sql,
        ],
        capture_output=True,
        text=True,
        env=env,
    )


def must_run(host: str, port: int, user: str, password: str, sql: str) -> None:
    res = run_psql(host, port, user, password, sql)
    if res.returncode != 0:
        combined = (res.stdout or "") + (res.stderr or "")
        raise RuntimeError(f"psql failed ({res.returncode}): {combined.strip()}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    args = parser.parse_args()

    host = "127.0.0.1"
    port = args.port
    user = args.user
    password = args.password

    # Insert enough rows to make a full index scan obvious.
    row_count = 1000
    values = ", ".join(f"({i}, 1)" for i in range(1, row_count + 1))
    must_run(
        host,
        port,
        user,
        password,
        f"INSERT INTO limit_pushdown_t(id, a) VALUES {values};",
    )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())


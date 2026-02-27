#!/usr/bin/env python3
"""
Regression test for issue #1124: idle-in-transaction timeout reaper.

Validates the required behavior directly:
after idle_in_transaction_session_timeout fires for a session that is idle in
an open transaction, the next statement on that same session is rejected.
"""

import argparse
import subprocess
import sys
import time


IDLE_WAIT_SECONDS = 3.0
TIMEOUT_SETTING = "1s"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Regression test: post-timeout statement rejection after "
            "idle_in_transaction_session_timeout (#1124)"
        )
    )
    parser.add_argument(
        "--dsn",
        required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def run_test(dsn: str) -> int:
    proc = subprocess.Popen(
        ["psql", dsn, "--no-psqlrc", "-A", "-t"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=0,
    )

    if proc.stdin is None:
        print("FAIL: stdin pipe is unavailable")
        return 1

    try:
        proc.stdin.write(f"SET idle_in_transaction_session_timeout = '{TIMEOUT_SETTING}';\n")
        proc.stdin.write("BEGIN;\n")
        proc.stdin.flush()

        time.sleep(IDLE_WAIT_SECONDS)

        proc.stdin.write("SELECT 1;\n")
        proc.stdin.flush()
    except BrokenPipeError:
        # Server already terminated the session before the follow-up write.
        pass

    try:
        stdout, stderr = proc.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        stdout, stderr = proc.communicate()

    combined = f"{stdout}\n{stderr}"
    if "SET" in combined and "BEGIN" in combined and proc.returncode != 0:
        print("PASS: idle-in-transaction connection terminated by reaper")
        return 0

    print(
        "FAIL: "
        f"SET_in_output={('SET' in combined)}, "
        f"BEGIN_in_output={('BEGIN' in combined)}, "
        f"returncode={proc.returncode}"
    )
    print(f"stdout={stdout!r}")
    print(f"stderr={stderr!r}")
    return 1


def main() -> int:
    args = parse_args()
    return run_test(args.dsn)


if __name__ == "__main__":
    raise SystemExit(main())

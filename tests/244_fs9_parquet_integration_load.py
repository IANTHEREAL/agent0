#!/usr/bin/env python3
"""Stage parquet/csv fixtures into embedded fs9 before running case 244."""

import argparse
import base64
import os
import subprocess
from pathlib import Path


FIXTURE_DIR = Path(__file__).with_name("parquet_testdata")
TARGET_ROOT = "tests/parquet_testdata"


def sql_string(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def must_run_sql(host: str, port: int, user: str, password: str, sql: str) -> None:
    env = os.environ.copy()
    env["PGPASSWORD"] = password
    result = subprocess.run(
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
        ],
        input=sql,
        text=True,
        capture_output=True,
        env=env,
    )
    if result.returncode != 0:
        combined = (result.stdout or "") + (result.stderr or "")
        raise RuntimeError(f"psql failed ({result.returncode}): {combined.strip()}")


def stage_file_sql(relative_name: str) -> str:
    payload = base64.b64encode((FIXTURE_DIR / relative_name).read_bytes()).decode("ascii")
    target_path = f"{TARGET_ROOT}/{relative_name}"
    return (
        f"SELECT fs9_write({sql_string(target_path)}, "
        f"decode($b64${payload}$b64$, 'base64'));"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    args = parser.parse_args()

    sql = f"""
CREATE EXTENSION IF NOT EXISTS fs9;
CREATE EXTENSION IF NOT EXISTS parquet;
SELECT CASE
    WHEN fs9_exists('{TARGET_ROOT}/') THEN fs9_remove('{TARGET_ROOT}/', true)
    ELSE 0
END;
{stage_file_sql("basic.parquet")}
{stage_file_sql("test_copy.csv")}
"""
    must_run_sql("127.0.0.1", args.port, args.user, args.password, sql)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

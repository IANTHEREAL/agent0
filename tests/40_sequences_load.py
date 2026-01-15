#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
from dataclasses import dataclass


@dataclass
class DbConfig:
    host: str
    port: int
    user: str
    password: str
    database: str = "postgres"


def run_psql(cfg: DbConfig, sql: str) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["PGPASSWORD"] = cfg.password
    return subprocess.run(
        [
            "psql",
            "-X",
            "-qAt",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            cfg.host,
            "-p",
            str(cfg.port),
            "-U",
            cfg.user,
            "-d",
            cfg.database,
            "-c",
            sql,
        ],
        capture_output=True,
        text=True,
        env=env,
    )


def must_stdout(cfg: DbConfig, sql: str) -> str:
    res = run_psql(cfg, sql)
    if res.returncode != 0:
        raise RuntimeError(f"psql failed ({res.returncode}): {res.stderr.strip()}")
    return res.stdout.strip()


def must_error(cfg: DbConfig, sql: str, contains: str) -> str:
    res = run_psql(cfg, sql)
    combined = (res.stdout or "") + (res.stderr or "")
    if res.returncode == 0:
        raise RuntimeError("expected psql to fail, but it succeeded")
    if contains not in combined:
        raise RuntimeError(f"expected error containing {contains!r}, got: {combined!r}")
    return combined


def assert_lines(actual: str, expected: list[str]) -> None:
    lines = [l for l in actual.splitlines() if l.strip() != ""]
    if lines != expected:
        raise RuntimeError(f"unexpected output lines: {lines!r} != {expected!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    args = parser.parse_args()

    cfg = DbConfig(host="127.0.0.1", port=args.port, user=args.user, password=args.password)

    # Cleanup from prior runs (should be idempotent).
    must_stdout(
        cfg,
        """
        DROP TABLE IF EXISTS seq_serial_t;
        DROP SEQUENCE IF EXISTS s1;
        DROP SEQUENCE IF EXISTS s2;
        """,
    )

    # Standalone sequences: nextval/currval/setval + is_called semantics + currval session cache behavior.
    out = must_stdout(
        cfg,
        """
        CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 1;
        SELECT nextval('s1');
        SELECT currval('s1');
        SELECT setval('s1', 10);
        SELECT currval('s1');
        SELECT nextval('s1');
        SELECT currval('s1');
        SELECT setval('s1', 20, false);
        SELECT currval('s1');
        SELECT nextval('s1');
        SELECT currval('s1');
        """,
    )
    assert_lines(out, ["1", "1", "10", "1", "11", "11", "20", "11", "20", "20"])

    # currval cross-connection behavior: session A defines currval, session B errors.
    out = must_stdout(
        cfg,
        """
        CREATE SEQUENCE s2 START WITH 1 INCREMENT BY 1;
        SELECT nextval('s2');
        SELECT currval('s2');
        """,
    )
    assert_lines(out, ["1", "1"])
    must_error(cfg, "SELECT currval('s2');", "not yet defined in this session")

    # SERIAL bridge: implicit {table}_{column}_seq exists, nextval/setval shares backing with INSERT-generated values,
    # and DROP TABLE cleans up owned implicit sequences.
    out = must_stdout(
        cfg,
        """
        DROP TABLE IF EXISTS seq_serial_t;
        CREATE TABLE seq_serial_t (id SERIAL PRIMARY KEY, v INT);
        SELECT relkind FROM pg_catalog.pg_class WHERE relname = 'seq_serial_t_id_seq';
        INSERT INTO seq_serial_t(v) VALUES (10),(20);
        SELECT max(id) FROM seq_serial_t;
        SELECT nextval('seq_serial_t_id_seq');
        INSERT INTO seq_serial_t(v) VALUES (30);
        SELECT max(id) FROM seq_serial_t;
        SELECT setval('seq_serial_t_id_seq', 100);
        INSERT INTO seq_serial_t(v) VALUES (40);
        SELECT max(id) FROM seq_serial_t;
        SELECT setval('seq_serial_t_id_seq', 200, false);
        INSERT INTO seq_serial_t(v) VALUES (50);
        SELECT max(id) FROM seq_serial_t;
        DROP TABLE seq_serial_t;
        SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'seq_serial_t_id_seq';
        """,
    )
    assert_lines(out, ["S", "2", "3", "4", "100", "101", "200", "200", "0"])

    # Cleanup.
    must_stdout(cfg, "DROP SEQUENCE IF EXISTS s1; DROP SEQUENCE IF EXISTS s2;")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as e:
        print(f"[40_sequences_load.py] FAILED: {e}", file=sys.stderr)
        raise


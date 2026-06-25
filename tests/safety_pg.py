#!/usr/bin/env python3
"""Small psycopg helpers for DB9 safety scripts."""

import sys

try:
    import psycopg
except ImportError:
    print(
        "ERROR: psycopg is required; run with `uv run --with 'psycopg[binary]'`",
        file=sys.stderr,
    )
    raise


def exec_sql(dsn, sql):
    with psycopg.connect(dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute(sql)


def exec_one(dsn, sql):
    with psycopg.connect(dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute(sql)
            row = cur.fetchone()
            return None if row is None else row[0]


def exec_all_text(dsn, sql):
    with psycopg.connect(dsn, autocommit=True) as conn:
        with conn.cursor() as cur:
            cur.execute(sql)
            rows = cur.fetchall()
            return "\n".join("|".join(str(value) for value in row) for row in rows)


def sql_error(fn):
    try:
        fn()
    except psycopg.Error as exc:
        return exc
    return None

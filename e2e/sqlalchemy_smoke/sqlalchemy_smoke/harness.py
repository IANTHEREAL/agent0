from __future__ import annotations

import os
from contextlib import contextmanager

from sqlalchemy import text
from sqlalchemy import create_engine
from sqlalchemy.engine import Engine
from sqlalchemy.pool import NullPool


SCHEMA_NAME = "e2e_sqlalchemy_smoke"


def _sqlalchemy_url_from_pg_dsn(pg_dsn: str) -> str:
    if pg_dsn.startswith("postgres://"):
        return "postgresql+psycopg://" + pg_dsn[len("postgres://") :]
    if pg_dsn.startswith("postgresql://"):
        return "postgresql+psycopg://" + pg_dsn[len("postgresql://") :]
    return pg_dsn


def engine_from_env() -> Engine:
    pg_dsn = os.environ.get("PG_DSN")
    if not pg_dsn:
        raise RuntimeError("PG_DSN env var is required")

    sqlalchemy_url = _sqlalchemy_url_from_pg_dsn(pg_dsn)
    return create_engine(sqlalchemy_url, poolclass=NullPool, use_native_hstore=False)


def _quote_ident(identifier: str) -> str:
    if not identifier:
        raise ValueError("identifier cannot be empty")
    return '"' + identifier.replace('"', '""') + '"'


def _drop_schema_cascade(engine: Engine, schema_name: str) -> None:
    schema = _quote_ident(schema_name)
    try:
        with engine.begin() as conn:
            conn.exec_driver_sql(f"DROP SCHEMA IF EXISTS {schema} CASCADE")
        return
    except Exception:
        pass

    with engine.begin() as conn:
        tables = conn.execute(
            text(
                "SELECT table_name FROM information_schema.tables "
                "WHERE table_schema = :schema ORDER BY table_name"
            ),
            {"schema": schema_name},
        ).fetchall()
        for (table_name,) in tables:
            table = _quote_ident(table_name)
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {schema}.{table} CASCADE")

    with engine.begin() as conn:
        conn.exec_driver_sql(f"DROP SCHEMA IF EXISTS {schema} CASCADE")


@contextmanager
def managed_schema(engine: Engine, schema_name: str = SCHEMA_NAME):
    _drop_schema_cascade(engine, schema_name)
    schema = _quote_ident(schema_name)
    with engine.begin() as conn:
        conn.exec_driver_sql(f"CREATE SCHEMA {schema}")

    try:
        yield schema_name
    finally:
        _drop_schema_cascade(engine, schema_name)

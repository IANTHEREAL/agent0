from __future__ import annotations

import os
import uuid
from contextlib import contextmanager

from flask import Flask
from flask_sqlalchemy import SQLAlchemy
from sqlalchemy import create_engine, text
from sqlalchemy.engine import Engine
from sqlalchemy.pool import NullPool


SCHEMA_NAME_PREFIX = "e2e_dify_sqlalchemy_compat"
SCHEMA_NAME = os.environ.get("DIFY_LITE_SCHEMA") or f"{SCHEMA_NAME_PREFIX}_{uuid.uuid4().hex[:12]}"
TZ_CONNECT_ARGS = {"options": "-c timezone=UTC"}


def _sqlalchemy_url_from_pg_dsn(pg_dsn: str) -> str:
    if pg_dsn.startswith("postgres://"):
        return "postgresql+psycopg2://" + pg_dsn[len("postgres://") :]
    if pg_dsn.startswith("postgresql://"):
        return "postgresql+psycopg2://" + pg_dsn[len("postgresql://") :]
    return pg_dsn


def engine_from_env() -> Engine:
    pg_dsn = os.environ.get("PG_DSN")
    if not pg_dsn:
        raise RuntimeError("PG_DSN env var is required")

    sqlalchemy_url = _sqlalchemy_url_from_pg_dsn(pg_dsn)
    return create_engine(sqlalchemy_url, poolclass=NullPool, connect_args=TZ_CONNECT_ARGS)


def flask_app_and_db_from_env() -> tuple[Flask, SQLAlchemy]:
    pg_dsn = os.environ.get("PG_DSN")
    if not pg_dsn:
        raise RuntimeError("PG_DSN env var is required")

    sqlalchemy_url = _sqlalchemy_url_from_pg_dsn(pg_dsn)
    app = Flask(__name__)
    app.config.update(
        SQLALCHEMY_DATABASE_URI=sqlalchemy_url,
        SQLALCHEMY_TRACK_MODIFICATIONS=False,
        SQLALCHEMY_ENGINE_OPTIONS={
            "poolclass": NullPool,
            "connect_args": TZ_CONNECT_ARGS,
        },
    )

    db = SQLAlchemy()
    db.init_app(app)
    return app, db


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

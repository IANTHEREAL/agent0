from __future__ import annotations

import pytest
from sqlalchemy import (
    Column,
    DateTime,
    ForeignKey,
    Index,
    Integer,
    MetaData,
    String,
    Table,
    UniqueConstraint,
    inspect,
    text,
)


def _quote_ident(identifier: str) -> str:
    if not identifier:
        raise ValueError("identifier cannot be empty")
    return '"' + identifier.replace('"', '""') + '"'


def _qname(schema: str, identifier: str) -> str:
    return f"{_quote_ident(schema)}.{_quote_ident(identifier)}"


def _relation_exists(conn, *, schema: str, name: str, relkind: str) -> bool:
    return (
        conn.execute(
            text(
                """
                SELECT 1
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = :schema
                  AND c.relname IN (:name)
                  AND c.relkind = :relkind
                LIMIT 1
                """
            ),
            {"schema": schema, "name": name, "relkind": relkind},
        ).scalar()
        is not None
    )


def test_add_column_backfill_then_set_not_null(schema, db):
    table = "migrations_add_column_backfill"
    qualified_table = _qname(schema, table)

    with db.engine.begin() as conn:
        conn.exec_driver_sql(
            f"CREATE TABLE {qualified_table} (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"
        )
        conn.exec_driver_sql(f"INSERT INTO {qualified_table} (id, name) VALUES (1, 'a'), (2, 'b')")

        conn.exec_driver_sql(f"ALTER TABLE {qualified_table} ADD COLUMN display_name TEXT")
        conn.exec_driver_sql(f"UPDATE {qualified_table} SET display_name = name WHERE display_name IS NULL")
        conn.exec_driver_sql(f"ALTER TABLE {qualified_table} ALTER COLUMN display_name SET NOT NULL")

        nullable = conn.execute(
            text(
                """
                SELECT is_nullable
                FROM information_schema.columns
                WHERE table_schema = :schema
                  AND table_name = :table
                  AND column_name = :column
                """
            ),
            {"schema": schema, "table": table, "column": "display_name"},
        ).scalar_one()
        null_rows = conn.execute(
            text(f"SELECT COUNT(*) FROM {qualified_table} WHERE display_name IS NULL")
        ).scalar_one()

    assert nullable == "NO"
    assert null_rows == 0

    with pytest.raises(Exception):
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"INSERT INTO {qualified_table} (id, name) VALUES (3, 'c')")


def test_drop_and_recreate_index_same_name_with_concurrently_desc(schema, db):
    try:
        suffix = schema[-8:]
        pg_schema = "public"
        table = f"migrations_index_lifecycle_{suffix}"
        qualified_table = _qname(pg_schema, table)
        index_name = f"idx_migrations_index_lifecycle_created_at_{suffix}"
        qualified_index = _qname(pg_schema, index_name)

        with db.engine.begin() as conn:
            conn.exec_driver_sql(
                f"CREATE TABLE {qualified_table} (id INTEGER PRIMARY KEY, created_at TIMESTAMPTZ NOT NULL)"
            )
            conn.exec_driver_sql(f'CREATE INDEX "{index_name}" ON {qualified_table} (created_at)')

        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP INDEX {qualified_index}")

        with db.engine.connect().execution_options(isolation_level="AUTOCOMMIT") as autocommit_conn:
            autocommit_conn.exec_driver_sql(
                f'CREATE INDEX CONCURRENTLY "{index_name}" ON {qualified_table} (created_at DESC)'
            )

        with db.engine.begin() as conn:
            assert _relation_exists(conn, schema=pg_schema, name=index_name, relkind="i")
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {qualified_table} CASCADE")


def test_rename_table_and_index_visibility_in_pg_class(schema, db):
    suffix = schema[-8:]
    pg_schema = "public"
    old_table = f"migrations_rename_src_table_{suffix}"
    new_table = f"migrations_rename_dst_table_{suffix}"
    old_index = f"idx_migrations_rename_src_value_{suffix}"
    new_index = f"idx_migrations_rename_dst_value_{suffix}"

    try:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(
                f"CREATE TABLE {_qname(pg_schema, old_table)} (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)"
            )
            conn.exec_driver_sql(f'CREATE INDEX "{old_index}" ON {_qname(pg_schema, old_table)} (value)')

        with db.engine.begin() as conn:
            conn.exec_driver_sql(
                f"ALTER TABLE {_qname(pg_schema, old_table)} RENAME TO {_quote_ident(new_table)}"
            )
            conn.exec_driver_sql(
                f"ALTER INDEX {_qname(pg_schema, old_index)} RENAME TO {_quote_ident(new_index)}"
            )

        with db.engine.begin() as conn:
            assert not _relation_exists(conn, schema=pg_schema, name=old_table, relkind="r")
            assert _relation_exists(conn, schema=pg_schema, name=new_table, relkind="r")

            # pg-tikv currently treats ALTER INDEX as a no-op; the index stays under
            # the original name (but should remain visible in pg_class).
            assert _relation_exists(conn, schema=pg_schema, name=old_index, relkind="i")
            assert not _relation_exists(conn, schema=pg_schema, name=new_index, relkind="i")
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, old_table)} CASCADE")
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, new_table)} CASCADE")


def test_sqlalchemy_inspector_reflection_smoke(schema, db):
    suffix = schema[-8:]
    pg_schema = "public"
    parent_table = f"inspector_parent_{suffix}"
    child_table = f"inspector_child_{suffix}"
    parent_code_uq = f"uq_inspector_parent_code_{suffix}"
    child_parent_slug_uq = f"uq_inspector_child_parent_slug_{suffix}"
    child_parent_fk = f"fk_inspector_child_parent_id_{suffix}"
    child_created_at_idx = f"ix_inspector_child_created_at_{suffix}"

    metadata = MetaData()

    parent = Table(
        parent_table,
        metadata,
        Column("id", Integer, primary_key=True),
        Column("code", String, nullable=False),
        UniqueConstraint("code", name=parent_code_uq),
        schema=pg_schema,
    )

    child = Table(
        child_table,
        metadata,
        Column("id", Integer, primary_key=True),
        Column(
            "parent_id",
            Integer,
            ForeignKey(f"{pg_schema}.{parent_table}.id", name=child_parent_fk),
            nullable=False,
        ),
        Column("slug", String, nullable=False),
        Column("created_at", DateTime(timezone=True), nullable=False),
        UniqueConstraint("parent_id", "slug", name=child_parent_slug_uq),
        schema=pg_schema,
    )

    Index(child_created_at_idx, child.c.created_at)

    try:
        metadata.create_all(db.engine)

        inspector = inspect(db.engine)

        assert inspector.has_table(parent_table, schema=pg_schema)
        assert inspector.has_table(child_table, schema=pg_schema)
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, child_table)} CASCADE")
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, parent_table)} CASCADE")

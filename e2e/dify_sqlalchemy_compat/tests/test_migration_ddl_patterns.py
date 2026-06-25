from __future__ import annotations

import time

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
from sqlalchemy.exc import OperationalError


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


def _is_serialization_failure(exc: OperationalError) -> bool:
    orig = getattr(exc, "orig", None)
    return getattr(orig, "pgcode", None) == "40001"


def _drop_table_if_exists_cascade_with_retry(db, qualified_table: str) -> None:
    for attempt in range(8):
        try:
            with db.engine.begin() as conn:
                conn.exec_driver_sql(f"DROP TABLE IF EXISTS {qualified_table} CASCADE")
            return
        except OperationalError as exc:
            if not _is_serialization_failure(exc) or attempt == 7:
                raise
            time.sleep(0.05 * (2**attempt))


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
        _drop_table_if_exists_cascade_with_retry(db, qualified_table)


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

            # ALTER INDEX RENAME is now implemented — index should appear under the new name.
            assert not _relation_exists(conn, schema=pg_schema, name=old_index, relkind="i")
            assert _relation_exists(conn, schema=pg_schema, name=new_index, relkind="i")
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

        # 1. has_table
        assert inspector.has_table(parent_table, schema=pg_schema)
        assert inspector.has_table(child_table, schema=pg_schema)

        # 2. get_pk_constraint
        pk = inspector.get_pk_constraint(child_table, schema=pg_schema)
        assert pk["constrained_columns"] == ["id"]
        assert pk["name"] is not None

        # 3. get_unique_constraints
        ucs = inspector.get_unique_constraints(child_table, schema=pg_schema)
        uc_names = {uc["name"] for uc in ucs}
        assert child_parent_slug_uq in uc_names
        uc = next(uc for uc in ucs if uc["name"] == child_parent_slug_uq)
        assert uc["column_names"] == ["parent_id", "slug"]

        # 4. get_foreign_keys
        fks = inspector.get_foreign_keys(child_table, schema=pg_schema)
        assert len(fks) == 1
        fk = fks[0]
        assert fk["constrained_columns"] == ["parent_id"]
        assert fk["referred_table"] == parent_table
        assert fk["referred_columns"] == ["id"]

        # 5. get_indexes
        idxs = inspector.get_indexes(child_table, schema=pg_schema)
        idx_names = {idx["name"] for idx in idxs}
        assert child_created_at_idx in idx_names

        # 6. get_sequence_names
        seqs = inspector.get_sequence_names(schema=pg_schema)
        assert isinstance(seqs, list)
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, child_table)} CASCADE")
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {_qname(pg_schema, parent_table)} CASCADE")


def test_alter_column_type_using_text_to_jsonb_updates_data_and_metadata(schema, db):
    table = "migrations_alter_type_using_jsonb"
    qualified_table = _qname(schema, table)

    try:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(
                f"CREATE TABLE {qualified_table} (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)"
            )
            conn.execute(
                text(f"INSERT INTO {qualified_table} (id, payload) VALUES (:id, :payload)"),
                {
                    "id": 1,
                    "payload": '{"a":"x","nested":{"b":"y"},"tags":["a","b"]}',
                },
            )

            before = conn.execute(
                text(
                    """
                    SELECT data_type, udt_name
                    FROM information_schema.columns
                    WHERE table_schema = :schema
                      AND table_name = :table
                      AND column_name = :column
                    """
                ),
                {"schema": schema, "table": table, "column": "payload"},
            ).one()

            conn.exec_driver_sql(
                f"ALTER TABLE {qualified_table} "
                "ALTER COLUMN payload TYPE JSONB USING payload::jsonb"
            )

            after = conn.execute(
                text(
                    """
                    SELECT data_type, udt_name
                    FROM information_schema.columns
                    WHERE table_schema = :schema
                      AND table_name = :table
                      AND column_name = :column
                    """
                ),
                {"schema": schema, "table": table, "column": "payload"},
            ).one()

            # Verify the USING clause applied a real cast (jsonb operators should work).
            stmt = text(
                f"SELECT payload #>> '{{nested,b}}' FROM {qualified_table} WHERE id = 1"
            )
            assert conn.execute(stmt).scalar_one() == "y"

            # Verify inserts after TYPE change are coerced to JSONB.
            conn.execute(
                text(f"INSERT INTO {qualified_table} (id, payload) VALUES (:id, :payload)"),
                {"id": 2, "payload": '{"a":"z","nested":{"b":"w"}}'},
            )
            stmt = text(f"SELECT payload ->> 'a' FROM {qualified_table} WHERE id = 2")
            assert conn.execute(stmt).scalar_one() == "z"
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {qualified_table} CASCADE")

    assert before.data_type != "jsonb"
    assert after.data_type == "jsonb"
    assert after.udt_name == "jsonb"


def test_serial_sequence_pg_get_serial_sequence_nextval_and_default_insert(schema, db):
    table = "migrations_serial_sequence"
    qualified_table = _qname(schema, table)
    expected_sequence = f"{schema}.{table}_id_seq"

    try:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(
                f"CREATE TABLE {qualified_table} (id SERIAL PRIMARY KEY, name TEXT NOT NULL)"
            )

            seq_unquoted = conn.execute(
                text("SELECT pg_get_serial_sequence(:table_name, :col_name)"),
                {"table_name": f"{schema}.{table}", "col_name": "id"},
            ).scalar()
            seq_quoted = conn.execute(
                text("SELECT pg_get_serial_sequence(:table_name, :col_name)"),
                {"table_name": f'\"{schema}\".\"{table}\"', "col_name": "id"},
            ).scalar()

            assert seq_unquoted == expected_sequence
            assert seq_quoted == expected_sequence

            conn.execute(
                text(f"INSERT INTO {qualified_table} (name) VALUES (:name)"),
                {"name": "a"},
            )
            inserted_id = conn.execute(
                text(f"SELECT id FROM {qualified_table} WHERE name = :name"),
                {"name": "a"},
            ).scalar_one()

            explicit_next = conn.execute(
                text(f"SELECT nextval('{expected_sequence}')")
            ).scalar_one()

            conn.execute(
                text(f"INSERT INTO {qualified_table} (name) VALUES (:name)"),
                {"name": "b"},
            )
            inserted_id_2 = conn.execute(
                text(f"SELECT id FROM {qualified_table} WHERE name = :name"),
                {"name": "b"},
            ).scalar_one()
    finally:
        with db.engine.begin() as conn:
            conn.exec_driver_sql(f"DROP TABLE IF EXISTS {qualified_table} CASCADE")

    assert inserted_id == 1
    assert explicit_next == 2
    assert inserted_id_2 == 3

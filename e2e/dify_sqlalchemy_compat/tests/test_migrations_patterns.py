from __future__ import annotations

import uuid

from sqlalchemy import Integer, String, select, text
from sqlalchemy.dialects import postgresql
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column


def test_uuid_ossp_uuid_generate_v4_server_default(schema, db):
    # Upstream: dify migration init uses `CREATE EXTENSION IF NOT EXISTS "uuid-ossp";`
    # and models use `server_default=sa.text("uuid_generate_v4()")`.
    with db.engine.begin() as conn:
        conn.exec_driver_sql('CREATE EXTENSION IF NOT EXISTS "uuid-ossp";')

    base = declarative_base()

    class UuidRow(base):
        __tablename__ = "uuid_default_rows"
        __table_args__ = {"schema": schema}

        id: Mapped[uuid.UUID] = mapped_column(
            postgresql.UUID(as_uuid=True),
            primary_key=True,
            server_default=text("uuid_generate_v4()"),
        )
        name: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add(UuidRow(name="x"))
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        generated = session.execute(select(UuidRow.id).where(UuidRow.name == "x")).scalar_one()

    parsed = uuid.UUID(str(generated))
    assert parsed.version == 4


def test_alter_table_drop_not_null_updates_information_schema(schema, db):
    base = declarative_base()

    class Plugin(base):
        __tablename__ = "plugins_nullable_ddl"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        declaration: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        before = conn.execute(
            text(
                """
                SELECT is_nullable
                FROM information_schema.columns
                WHERE table_schema = :schema
                  AND table_name = :table
                  AND column_name = :column
                """
            ),
            {"schema": schema, "table": "plugins_nullable_ddl", "column": "declaration"},
        ).scalar_one()

        conn.exec_driver_sql(
            f'ALTER TABLE "{schema}"."plugins_nullable_ddl" ALTER COLUMN declaration DROP NOT NULL'
        )

        after = conn.execute(
            text(
                """
                SELECT is_nullable
                FROM information_schema.columns
                WHERE table_schema = :schema
                  AND table_name = :table
                  AND column_name = :column
                """
            ),
            {"schema": schema, "table": "plugins_nullable_ddl", "column": "declaration"},
        ).scalar_one()

    assert before == "NO"
    assert after == "YES"

    with Session(db.engine, expire_on_commit=False) as session:
        session.execute(
            text(f'INSERT INTO "{schema}"."plugins_nullable_ddl" (id, declaration) VALUES (:id, :decl)'),
            {"id": 1, "decl": None},
        )
        session.commit()

from __future__ import annotations

from sqlalchemy import Column, Integer, MetaData, String, Table, delete, insert, select, update


def test_insert_update_delete_returning_core(schema, db):
    metadata = MetaData(schema=schema)
    items = Table(
        "items_returning_core",
        metadata,
        Column("id", Integer, primary_key=True),
        Column("name", String, nullable=False),
    )
    metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        rows = conn.execute(
            insert(items)
            .values(
                [
                    {"id": 1, "name": "a"},
                    {"id": 2, "name": "b"},
                ]
            )
            .returning(items.c.id, items.c.name)
        ).all()
        assert rows == [(1, "a"), (2, "b")]

        row = conn.execute(
            update(items)
            .where(items.c.id == 2)
            .values(name="b2")
            .returning(items.c.id, items.c.name)
        ).one()
        assert row == (2, "b2")

        row = conn.execute(
            delete(items).where(items.c.id == 1).returning(items.c.id, items.c.name)
        ).one()
        assert row == (1, "a")

        remaining = conn.execute(select(items.c.id, items.c.name).order_by(items.c.id)).all()
        assert remaining == [(2, "b2")]


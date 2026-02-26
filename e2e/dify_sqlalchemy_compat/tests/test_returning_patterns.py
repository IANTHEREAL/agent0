from __future__ import annotations

from sqlalchemy import Column, Integer, MetaData, String, Table, delete, insert, select, update
from sqlalchemy.dialects.postgresql import insert as pg_insert


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


def test_upsert_on_conflict_do_update_returning(schema, db):
    metadata = MetaData(schema=schema)
    products = Table(
        "products_upsert_ret",
        metadata,
        Column("id", Integer, primary_key=True),
        Column("name", String, nullable=False),
        Column("price", Integer, nullable=False),
    )
    metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        # Seed one row
        conn.execute(insert(products).values(id=1, name="widget", price=100))

        # Upsert: conflict on id=1 (update), new row id=2 (insert)
        stmt = (
            pg_insert(products)
            .values([{"id": 1, "name": "widget", "price": 150}, {"id": 2, "name": "gadget", "price": 200}])
            .on_conflict_do_update(
                index_elements=["id"],
                set_={"price": pg_insert(products).excluded.price},
            )
            .returning(products.c.id, products.c.name, products.c.price)
        )
        rows = sorted(conn.execute(stmt).all(), key=lambda r: r[0])
        assert rows == [(1, "widget", 150), (2, "gadget", 200)]


def test_multi_row_update_returning(schema, db):
    metadata = MetaData(schema=schema)
    scores = Table(
        "scores_upd_ret",
        metadata,
        Column("id", Integer, primary_key=True),
        Column("value", Integer, nullable=False),
    )
    metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        conn.execute(
            insert(scores).values([{"id": 1, "value": 10}, {"id": 2, "value": 20}, {"id": 3, "value": 30}])
        )

        rows = conn.execute(
            update(scores)
            .where(scores.c.id.in_([1, 3]))
            .values(value=scores.c.value + 100)
            .returning(scores.c.id, scores.c.value)
        ).all()
        rows = sorted(rows, key=lambda r: r[0])
        assert rows == [(1, 110), (3, 130)]


def test_multi_row_delete_returning(schema, db):
    metadata = MetaData(schema=schema)
    logs = Table(
        "logs_del_ret",
        metadata,
        Column("id", Integer, primary_key=True),
        Column("msg", String, nullable=False),
    )
    metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        conn.execute(
            insert(logs).values([{"id": 1, "msg": "a"}, {"id": 2, "msg": "b"}, {"id": 3, "msg": "c"}])
        )

        rows = conn.execute(
            delete(logs).where(logs.c.id < 3).returning(logs.c.id, logs.c.msg)
        ).all()
        rows = sorted(rows, key=lambda r: r[0])
        assert rows == [(1, "a"), (2, "b")]

        remaining = conn.execute(select(logs.c.id, logs.c.msg).order_by(logs.c.id)).all()
        assert remaining == [(3, "c")]


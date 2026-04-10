from __future__ import annotations

from datetime import datetime

from sqlalchemy import DateTime, Integer, String, cast, extract, func, literal_column, select
from sqlalchemy.dialects.postgresql import JSONB, insert
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

from sqlalchemy_smoke.harness import SCHEMA_NAME, engine_from_env, managed_schema


def test_sqlalchemy_smoke():
    engine = engine_from_env()

    try:
        with managed_schema(engine, SCHEMA_NAME):
            base = declarative_base()

            class Item(base):
                __tablename__ = "items"
                __table_args__ = {"schema": SCHEMA_NAME}

                id: Mapped[int] = mapped_column(Integer, primary_key=True)
                name: Mapped[str] = mapped_column(String, nullable=False)
                payload: Mapped[dict] = mapped_column(JSONB, nullable=False)
                occurred_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)

            base.metadata.create_all(engine)

            with Session(engine) as session:
                session.add(
                    Item(
                        id=1,
                        name="alpha",
                        payload={"kind": "alpha"},
                        occurred_at=datetime(2020, 1, 1, 12, 0, 0),
                    )
                )
                session.commit()

            with Session(engine) as session:
                item = session.get(Item, 1)
                assert item is not None
                assert item.name == "alpha"

                item.name = "alpha2"
                session.commit()

            with Session(engine) as session:
                item = session.get(Item, 1)
                assert item is not None
                assert item.name == "alpha2"

                session.delete(item)
                session.commit()

            with Session(engine) as session:
                assert session.get(Item, 1) is None

            with Session(engine) as session:
                session.add(
                    Item(
                        id=2,
                        name="rollback",
                        payload={"kind": "rollback"},
                        occurred_at=datetime(2020, 1, 1, 12, 1, 0),
                    )
                )
                session.flush()
                session.rollback()

            with Session(engine) as session:
                assert session.get(Item, 2) is None

            with Session(engine) as session:
                pending = Item(
                    id=3,
                    name="txn_visible",
                    payload={"kind": "txn", "phase": "inserted"},
                    occurred_at=datetime(2020, 1, 1, 12, 1, 30),
                )
                session.add(pending)
                session.flush()

                inserted = session.execute(
                    select(Item.id, Item.name).where(Item.name == "txn_visible")
                ).all()
                assert inserted == [(3, "txn_visible")]

                pending.name = "txn_updated"
                pending.payload = {"kind": "txn", "phase": "updated"}
                session.flush()

                updated = session.execute(
                    select(Item.name, Item.payload).where(Item.id == 3)
                ).one()
                assert updated == ("txn_updated", {"kind": "txn", "phase": "updated"})

                session.delete(pending)
                session.flush()

                deleted_count = session.execute(
                    select(func.count()).select_from(Item).where(Item.id == 3)
                ).scalar_one()
                assert deleted_count == 0

                session.rollback()

            with Session(engine) as session:
                assert session.get(Item, 3) is None

            with Session(engine) as session:
                session.add_all(
                    [
                        Item(
                            id=10,
                            name="jsonb_1",
                            payload={"a": {"b": 1}, "kind": "jsonb"},
                            occurred_at=datetime(2020, 1, 1, 12, 2, 0),
                        ),
                        Item(
                            id=11,
                            name="jsonb_2",
                            payload={"a": {"b": 2}, "kind": "jsonb"},
                            occurred_at=datetime(2020, 1, 1, 12, 3, 0),
                        ),
                    ]
                )
                session.commit()

            with Session(engine) as session:
                jsonb_ids = session.execute(
                    select(Item.id)
                    .where(Item.payload.contains({"a": {"b": 1}}))
                    .order_by(Item.id)
                ).scalars()
                assert list(jsonb_ids) == [10]

            with Session(engine) as session:
                session.execute(
                    insert(Item),
                    [
                        {
                            "id": 30,
                            "name": "write_1",
                            "payload": {"kind": "write", "phase": "inserted"},
                            "occurred_at": datetime(2020, 1, 1, 12, 4, 0),
                        },
                        {
                            "id": 31,
                            "name": "write_2",
                            "payload": {"kind": "write", "phase": "inserted"},
                            "occurred_at": datetime(2020, 1, 1, 12, 5, 0),
                        },
                    ],
                )
                session.commit()

            with Session(engine) as session:
                upsert_stmt = insert(Item).values(
                    id=30,
                    name="write_1_upserted",
                    payload={"kind": "write", "phase": "upserted"},
                    occurred_at=datetime(2020, 1, 1, 12, 6, 0),
                )
                upsert_stmt = upsert_stmt.on_conflict_do_update(
                    index_elements=[Item.id],
                    set_={
                        "name": upsert_stmt.excluded.name,
                        "payload": upsert_stmt.excluded.payload,
                        "occurred_at": upsert_stmt.excluded.occurred_at,
                    },
                )
                session.execute(upsert_stmt)

                write_2 = session.get(Item, 31)
                assert write_2 is not None
                write_2.name = "write_2_updated"
                write_2.payload = {"kind": "write", "phase": "updated"}
                session.flush()
                session.delete(write_2)
                session.commit()

            with Session(engine) as session:
                final_rows = session.execute(
                    select(Item).where(Item.id.in_([1, 30, 31])).order_by(Item.id)
                ).scalars().all()
                assert [(row.id, row.name) for row in final_rows] == [
                    (30, "write_1_upserted"),
                ]
                assert final_rows[0].payload == {"kind": "write", "phase": "upserted"}

                final_count = session.execute(
                    select(func.count()).select_from(Item).where(Item.id.in_([1, 30, 31]))
                ).scalar_one()
                assert final_count == 1

            with Session(engine) as session:
                session.add_all(
                    [
                        Item(
                            id=20,
                            name="agg_1",
                            payload={},
                            occurred_at=datetime(2020, 1, 1, 0, 30, 0),
                        ),
                        Item(
                            id=21,
                            name="agg_2",
                            payload={},
                            occurred_at=datetime(2020, 1, 1, 23, 50, 0),
                        ),
                        Item(
                            id=22,
                            name="agg_3",
                            payload={},
                            occurred_at=datetime(2020, 1, 2, 0, 10, 0),
                        ),
                    ]
                )
                session.commit()

            with Session(engine) as session:
                utc = literal_column("'UTC'")
                utc_local = cast(Item.occurred_at, DateTime).op("AT TIME ZONE")(utc)
                target_local = utc_local.op("AT TIME ZONE")(utc)
                year = cast(extract("year", target_local), Integer)
                month = cast(extract("month", target_local), Integer)
                day = cast(extract("day", target_local), Integer)
                rows = session.execute(
                    select(year, month, day, func.count())
                    .where(Item.id.in_([20, 21, 22]))
                    .group_by(year, month, day)
                    .order_by(year, month, day)
                ).all()
                assert rows == [(2020, 1, 1, 2), (2020, 1, 2, 1)]
    finally:
        engine.dispose()

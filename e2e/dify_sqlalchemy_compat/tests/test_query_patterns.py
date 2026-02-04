from __future__ import annotations

from datetime import datetime

from sqlalchemy import DateTime, Integer, String, func, select, text
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column


def test_distinct_on_order_by_latest_per_group(schema, db):
    base = declarative_base()

    class Event(base):
        __tablename__ = "events_distinct_on"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        group_key: Mapped[str] = mapped_column(String, nullable=False)
        created_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)
        payload: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all(
            [
                Event(id=1, group_key="a", created_at=datetime(2020, 1, 1, 0, 0, 0), payload="old"),
                Event(id=2, group_key="a", created_at=datetime(2020, 1, 2, 0, 0, 0), payload="new"),
                Event(id=3, group_key="b", created_at=datetime(2020, 1, 1, 0, 0, 0), payload="only"),
            ]
        )
        session.commit()

    stmt = (
        select(Event.group_key, Event.payload)
        .distinct(Event.group_key)
        .order_by(Event.group_key, Event.created_at.desc())
    )

    with Session(db.engine, expire_on_commit=False) as session:
        rows = session.execute(stmt).all()

    assert rows == [("a", "new"), ("b", "only")]


def test_count_distinct(schema, db):
    base = declarative_base()

    class AggRow(base):
        __tablename__ = "agg_count_distinct"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        category: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all(
            [
                AggRow(id=1, category="a"),
                AggRow(id=2, category="a"),
                AggRow(id=3, category="b"),
            ]
        )
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        distinct_categories = session.execute(select(func.count(func.distinct(AggRow.category)))).scalar_one()

    assert distinct_categories == 2


def test_pagination_limit_offset_and_count(schema, db):
    base = declarative_base()

    class Doc(base):
        __tablename__ = "docs_pagination"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        title: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all([Doc(id=i, title=f"doc-{i:02d}") for i in range(1, 26)])
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        total = session.execute(select(func.count()).select_from(Doc)).scalar_one()
        page = list(
            session.execute(select(Doc.id).order_by(Doc.id).limit(10).offset(10)).scalars()
        )

    assert total == 25
    assert page == list(range(11, 21))


def test_text_tuple_bind_for_in_clause(schema, db):
    base = declarative_base()

    class Thing(base):
        __tablename__ = "things_in_tuple_bind"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        name: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all([Thing(id=i, name=f"t{i}") for i in range(1, 6)])
        session.commit()

    stmt = text(f'SELECT id FROM "{schema}"."things_in_tuple_bind" WHERE id IN :ids ORDER BY id')

    with Session(db.engine, expire_on_commit=False) as session:
        ids = (1, 3, 5)
        rows = list(session.execute(stmt, {"ids": ids}).scalars())

    assert rows == [1, 3, 5]

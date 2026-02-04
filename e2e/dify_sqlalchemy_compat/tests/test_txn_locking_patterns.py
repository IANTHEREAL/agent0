from __future__ import annotations

import threading
import time

from sqlalchemy import Integer, String, select
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column


def test_for_update_skip_locked_with_limit(schema, db):
    base = declarative_base()

    class QueueItem(base):
        __tablename__ = "queue_items_skip_locked"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        payload: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all([QueueItem(id=1, payload="a"), QueueItem(id=2, payload="b")])
        session.commit()

    lock_first_stmt = select(QueueItem.id).order_by(QueueItem.id).with_for_update().limit(1)
    skip_locked_stmt = (
        select(QueueItem.id).order_by(QueueItem.id).with_for_update(skip_locked=True).limit(1)
    )

    engine = db.engine
    thread_started = threading.Event()
    result: dict[str, object] = {}

    def run_skip_locked_select() -> None:
        try:
            with Session(engine, expire_on_commit=False) as session, session.begin():
                thread_started.set()
                result["id"] = session.execute(skip_locked_stmt).scalar_one()
        except Exception as exc:  # pragma: no cover - surfaced by assertion below
            result["exc"] = exc

    with Session(db.engine, expire_on_commit=False) as session:
        with session.begin():
            locked_id = session.execute(lock_first_stmt).scalar_one()
            thread = threading.Thread(target=run_skip_locked_select, daemon=True)
            thread.start()
            assert thread_started.wait(1.0)
            time.sleep(0.25)
        # Commit releases the row lock; if SKIP LOCKED is not implemented, the thread will have
        # blocked until this point and then return the locked row.

    thread.join(timeout=2.0)
    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result.get("id") != locked_id


def test_begin_nested_savepoint_rollback_keeps_outer_txn(schema, db):
    base = declarative_base()

    class Item(base):
        __tablename__ = "items_savepoint"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        with session.begin():
            session.add(Item(id=1, value="outer"))
            try:
                with session.begin_nested():
                    session.add(Item(id=2, value="nested"))
                    session.flush()
                    raise RuntimeError("rollback nested")
            except RuntimeError:
                pass
            session.add(Item(id=3, value="after"))

    with Session(db.engine, expire_on_commit=False) as session:
        ids = list(session.execute(select(Item.id).order_by(Item.id)).scalars())

    assert ids == [1, 3]

from __future__ import annotations

import threading
import time

import pytest
from sqlalchemy import Integer, String, select, text
from sqlalchemy.exc import OperationalError, SQLAlchemyError
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

NOWAIT_IMMEDIATE_TIMEOUT_SECONDS = 1.0
SYNC_WAIT_TIMEOUT_SECONDS = 5.0
FOR_SHARE_SKIP_LOCKED_XFAIL_SQLSTATES = {"55P03"}
FOR_SHARE_SKIP_LOCKED_XFAIL_MESSAGE_MARKERS = (
    "lock wait timeout",
    "timeout while waiting for lock",
    "could not obtain lock",
    "lock not available",
)


def _wait_for_signal(event: threading.Event, *, timeout_s: float, reason: str) -> None:
    if event.wait(timeout=timeout_s):
        return
    raise AssertionError(
        f"Thread synchronization precondition failed: {reason} (timeout={timeout_s:.1f}s)"
    )


def _is_for_share_skip_locked_divergence(pgcode: str | None, message: str) -> bool:
    if pgcode in FOR_SHARE_SKIP_LOCKED_XFAIL_SQLSTATES:
        return True
    lowered = message.lower()
    has_lock_marker = any(
        marker in lowered for marker in FOR_SHARE_SKIP_LOCKED_XFAIL_MESSAGE_MARKERS
    )
    # db9 may currently surface lock contention as 40001 instead of 55P03.
    if pgcode == "40001":
        return has_lock_marker
    if pgcode is None:
        return has_lock_marker
    return False


def _is_for_share_skip_locked_row_divergence(ids: object) -> bool:
    # Keep xfail scope narrow: only treat the known "contended row leaks through"
    # shape as divergence. Any other row set mismatch must fail loudly.
    return ids == [1, 2, 3]


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


# ---------------------------------------------------------------------------
# Tests 1–8 + stretch: NOWAIT, FOR SHARE, SKIP LOCKED  (issue #1116)
# ---------------------------------------------------------------------------


def test_nowait_raises_55p03_on_contended_row(schema, db):
    """NOWAIT on a row locked by another session must raise 55P03 immediately."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "nowait_test"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b")])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    b_ready = threading.Event()
    b_query_started = threading.Event()
    b_done = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            with Session(engine) as s, s.begin():
                b_ready.set()
                _wait_for_signal(
                    lock_acquired,
                    timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                    reason="Session B waiting for Session A to acquire row 1 lock",
                )
                stmt = select(Row).where(Row.id == 1).with_for_update(nowait=True)
                started_at = time.monotonic()
                try:
                    b_query_started.set()
                    s.execute(stmt).one()
                finally:
                    result["elapsed_s"] = time.monotonic() - started_at
        except OperationalError as exc:
            result["pgcode"] = exc.orig.pgcode
        except Exception as exc:
            result["exc"] = exc
        finally:
            b_done.set()

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()
    assert b_ready.wait(timeout=SYNC_WAIT_TIMEOUT_SECONDS), (
        "Session B failed to initialize before lock contention was introduced"
    )

    with Session(db.engine) as s, s.begin():
        s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
        lock_acquired.set()
        _wait_for_signal(
            b_query_started,
            timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
            reason="Session B reaching NOWAIT execution point on row 1",
        )
        assert b_done.wait(timeout=SYNC_WAIT_TIMEOUT_SECONDS), (
            "Session B did not finish after reaching NOWAIT execution point "
            "while Session A held row 1 lock"
        )

    thread.join(timeout=5.0)
    assert not thread.is_alive()
    assert "exc" not in result, f"Session B unexpected exception: {result['exc']!r}"
    elapsed_s = result.get("elapsed_s")
    assert isinstance(elapsed_s, float), f"Missing NOWAIT latency measurement: {result}"
    assert elapsed_s <= NOWAIT_IMMEDIATE_TIMEOUT_SECONDS, (
        "NOWAIT should not block on contention: "
        f"observed latency={elapsed_s:.3f}s (threshold={NOWAIT_IMMEDIATE_TIMEOUT_SECONDS:.3f}s)"
    )
    assert result.get("pgcode") == "55P03", f"Expected 55P03, got {result}"


def test_nowait_succeeds_on_uncontended_row(schema, db):
    """NOWAIT succeeds when the target row is not locked by anyone else."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "nowait_uncontended"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b")])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    b_ready = threading.Event()
    b_done = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            with Session(engine) as s, s.begin():
                b_ready.set()
                _wait_for_signal(
                    lock_acquired,
                    timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                    reason="Session B waiting for Session A to acquire row 1 lock",
                )
                # Row 2 is NOT locked by session A → should succeed
                stmt = select(Row).where(Row.id == 2).with_for_update(nowait=True)
                row = s.execute(stmt).one()
                result["id"] = row[0].id
                result["value"] = row[0].value
        except Exception as exc:
            result["exc"] = exc
        finally:
            b_done.set()

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()
    assert b_ready.wait(timeout=SYNC_WAIT_TIMEOUT_SECONDS), (
        "Session B failed to initialize before lock contention was introduced"
    )

    finished_while_locked = False
    with Session(db.engine) as s, s.begin():
        s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
        lock_acquired.set()
        finished_while_locked = b_done.wait(timeout=2.0)
        assert finished_while_locked, (
            "Session B should finish while Session A holds an unrelated row lock"
        )

    thread.join(timeout=5.0)
    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result["id"] == 2
    assert result["value"] == "b"


def test_for_share_locks_row_against_for_update_nowait(schema, db):
    """FOR SHARE blocks a concurrent FOR UPDATE NOWAIT on the same row.

    db9 deviation: TiKV has no shared row-level lock, so FOR SHARE is
    upgraded to an exclusive (FOR UPDATE) lock internally. Two concurrent
    FOR SHARE holders on the same row will therefore conflict — unlike
    PostgreSQL where shared locks are compatible.
    """
    base = declarative_base()

    class Row(base):
        __tablename__ = "for_share_test"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b")])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    b_ready = threading.Event()
    b_query_started = threading.Event()
    b_done = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            with Session(engine) as s, s.begin():
                b_ready.set()
                _wait_for_signal(
                    lock_acquired,
                    timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                    reason="Session B waiting for Session A FOR SHARE lock on row 1",
                )
                stmt = select(Row).where(Row.id == 1).with_for_update(nowait=True)
                started_at = time.monotonic()
                try:
                    b_query_started.set()
                    s.execute(stmt).one()
                finally:
                    result["elapsed_s"] = time.monotonic() - started_at
        except OperationalError as exc:
            result["pgcode"] = exc.orig.pgcode
        except Exception as exc:
            result["exc"] = exc
        finally:
            b_done.set()

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()
    assert b_ready.wait(timeout=SYNC_WAIT_TIMEOUT_SECONDS), (
        "Session B failed to initialize before lock contention was introduced"
    )

    with Session(db.engine) as s, s.begin():
        # FOR SHARE — internally upgraded to exclusive in db9
        stmt_share = select(Row).where(Row.id == 1).with_for_update(read=True)
        row = s.execute(stmt_share).one()
        assert row[0].id == 1
        lock_acquired.set()
        _wait_for_signal(
            b_query_started,
            timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
            reason="Session B reaching NOWAIT execution point under FOR SHARE contention",
        )
        assert b_done.wait(timeout=SYNC_WAIT_TIMEOUT_SECONDS), (
            "Session B did not finish after reaching NOWAIT execution point "
            "while Session A held FOR SHARE lock on row 1"
        )

    thread.join(timeout=5.0)
    assert not thread.is_alive()
    assert "exc" not in result, f"Session B unexpected exception: {result['exc']!r}"
    elapsed_s = result.get("elapsed_s")
    assert isinstance(elapsed_s, float), f"Missing NOWAIT latency measurement: {result}"
    assert elapsed_s <= NOWAIT_IMMEDIATE_TIMEOUT_SECONDS, (
        "NOWAIT should not block on contention: "
        f"observed latency={elapsed_s:.3f}s (threshold={NOWAIT_IMMEDIATE_TIMEOUT_SECONDS:.3f}s)"
    )
    assert result.get("pgcode") == "55P03", f"Expected 55P03, got {result}"


def test_skip_locked_returns_only_unlocked_rows(schema, db):
    """SKIP LOCKED returns exactly the rows not held by another session."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "skip_locked_exact"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([
            Row(id=1, value="a"),
            Row(id=2, value="b"),
            Row(id=3, value="c"),
            Row(id=4, value="d"),
        ])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            _wait_for_signal(
                lock_acquired,
                timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                reason="Session B waiting for Session A to lock rows [1, 2]",
            )
            with Session(engine) as s, s.begin():
                stmt = (
                    select(Row.id)
                    .order_by(Row.id)
                    .with_for_update(skip_locked=True)
                )
                result["ids"] = list(s.execute(stmt).scalars())
        except Exception as exc:
            result["exc"] = exc

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()

    with Session(db.engine) as s, s.begin():
        # Lock rows 1 and 2
        s.execute(
            select(Row).where(Row.id.in_([1, 2])).order_by(Row.id).with_for_update()
        ).all()
        lock_acquired.set()
        thread.join(timeout=5.0)

    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result["ids"] == [3, 4]


def test_skip_locked_empty_when_all_rows_locked(schema, db):
    """SKIP LOCKED returns an empty result set (not an error) when every row is locked."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "skip_locked_all"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b"), Row(id=3, value="c")])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            _wait_for_signal(
                lock_acquired,
                timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                reason="Session B waiting for Session A to lock all rows",
            )
            with Session(engine) as s, s.begin():
                stmt = (
                    select(Row.id)
                    .order_by(Row.id)
                    .with_for_update(skip_locked=True)
                )
                result["ids"] = list(s.execute(stmt).scalars())
        except Exception as exc:
            result["exc"] = exc

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()

    with Session(db.engine) as s, s.begin():
        # Lock ALL rows
        s.execute(select(Row).order_by(Row.id).with_for_update()).all()
        lock_acquired.set()
        thread.join(timeout=5.0)

    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result["ids"] == []


def test_skip_locked_with_limit(schema, db):
    """SKIP LOCKED + LIMIT returns the correct count of non-locked rows."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "skip_locked_limit"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([
            Row(id=1, value="a"),
            Row(id=2, value="b"),
            Row(id=3, value="c"),
            Row(id=4, value="d"),
        ])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            _wait_for_signal(
                lock_acquired,
                timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                reason="Session B waiting for Session A to lock rows [1, 2]",
            )
            with Session(engine) as s, s.begin():
                stmt = (
                    select(Row.id)
                    .order_by(Row.id)
                    .with_for_update(skip_locked=True)
                    .limit(1)
                )
                result["ids"] = list(s.execute(stmt).scalars())
        except Exception as exc:
            result["exc"] = exc

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()

    with Session(db.engine) as s, s.begin():
        # Lock rows 1 and 2
        s.execute(
            select(Row).where(Row.id.in_([1, 2])).order_by(Row.id).with_for_update()
        ).all()
        lock_acquired.set()
        thread.join(timeout=5.0)

    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result["ids"] == [3]


def test_nowait_succeeds_after_lock_released(schema, db):
    """After the lock holder commits, NOWAIT on the same row succeeds."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "nowait_released"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a")])
        s.commit()

    engine = db.engine
    committed = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            _wait_for_signal(
                committed,
                timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                reason="Session B waiting for Session A commit",
            )
            with Session(engine) as s, s.begin():
                stmt = select(Row).where(Row.id == 1).with_for_update(nowait=True)
                row = s.execute(stmt).one()
                result["id"] = row[0].id
                result["value"] = row[0].value
        except Exception as exc:
            result["exc"] = exc

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()

    # Lock then release
    with Session(db.engine) as s, s.begin():
        s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
    # Transaction committed — lock released
    committed.set()

    thread.join(timeout=5.0)
    assert not thread.is_alive()
    assert "exc" not in result, result.get("exc")
    assert result["id"] == 1
    assert result["value"] == "a"


def test_for_share_skip_locked_divergence(schema, db):
    """FOR SHARE SKIP LOCKED should skip contended rows (PostgreSQL semantics)."""
    base = declarative_base()

    class Row(base):
        __tablename__ = "for_share_skip_locked"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b"), Row(id=3, value="c")])
        s.commit()

    engine = db.engine
    lock_acquired = threading.Event()
    b_query_started = threading.Event()
    b_done = threading.Event()
    result: dict[str, object] = {}

    def session_b() -> None:
        try:
            _wait_for_signal(
                lock_acquired,
                timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
                reason="Session B waiting for Session A exclusive lock on row 1",
            )
            with Session(engine) as s, s.begin():
                # PG semantics: skip row 1 if it is locked by Session A.
                stmt = (
                    select(Row.id)
                    .order_by(Row.id)
                    .with_for_update(read=True, skip_locked=True)
                )
                b_query_started.set()
                result["ids"] = list(s.execute(stmt).scalars())
        except Exception as exc:
            result["exc"] = exc
        finally:
            b_done.set()

    thread = threading.Thread(target=session_b, daemon=True)
    thread.start()

    finished_while_locked = False
    with Session(db.engine) as s, s.begin():
        # Lock row 1 exclusively
        s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
        lock_acquired.set()
        _wait_for_signal(
            b_query_started,
            timeout_s=SYNC_WAIT_TIMEOUT_SECONDS,
            reason="Session B reaching FOR SHARE SKIP LOCKED execution point",
        )
        # Under PostgreSQL semantics, session B should return immediately
        # with ids [2, 3] while this lock is still held.
        finished_while_locked = b_done.wait(timeout=NOWAIT_IMMEDIATE_TIMEOUT_SECONDS)

    thread.join(timeout=5.0)
    assert not thread.is_alive()
    if "exc" in result:
        exc = result["exc"]
        orig = getattr(exc, "orig", None)
        pgcode = getattr(orig, "pgcode", None)
        message = str(orig if orig is not None else exc)

        # Allow xfail only for clear lock-contention-shaped divergence.
        if _is_for_share_skip_locked_divergence(pgcode, message):
            pytest.xfail(
                "db9 divergence: FOR SHARE SKIP LOCKED raised lock-contention error "
                f"instead of returning unlocked rows (pgcode={pgcode!r}, message={message!r})"
            )
        raise AssertionError(
            "Unexpected non-lock SQL error in FOR SHARE SKIP LOCKED test: "
            f"pgcode={pgcode!r}, message={message!r}"
        )

    ids = result.get("ids")
    if not finished_while_locked:
        pytest.xfail(
            "db9 divergence: FOR SHARE SKIP LOCKED blocks on a contended row "
            f"(observed ids after lock release: {ids!r})"
        )
    if ids != [2, 3]:
        if _is_for_share_skip_locked_row_divergence(ids):
            pytest.xfail(
                "db9 divergence: FOR SHARE SKIP LOCKED returned the contended row "
                f"instead of skipping it (expected [2, 3], got {ids!r})"
            )
        raise AssertionError(
            "Unexpected FOR SHARE SKIP LOCKED row mismatch. "
            f"Expected [2, 3], got {ids!r}"
        )


def test_deadlock_detection(schema, db):
    """Stretch: two sessions create a deadlock; at least one should error.

    TiKV has a deadlock detector, but db9 does not yet map the resulting
    error to PostgreSQL's 40P01 (deadlock_detected) SQLSTATE. This test
    documents current behavior.
    """
    base = declarative_base()

    class Row(base):
        __tablename__ = "deadlock_test"
        __table_args__ = {"schema": schema}
        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        value: Mapped[str] = mapped_column(String, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine) as s:
        s.add_all([Row(id=1, value="a"), Row(id=2, value="b")])
        s.commit()

    engine = db.engine
    a_locked = threading.Event()
    b_locked = threading.Event()
    result_a: dict[str, object] = {}
    result_b: dict[str, object] = {}

    def session_a() -> None:
        try:
            with Session(engine) as s:
                with s.begin():
                    s.execute(text("SET LOCAL lock_timeout = '5s'"))
                    s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
                    a_locked.set()
                    if not b_locked.wait(timeout=5.0):
                        raise AssertionError(
                            "Session A timed out waiting for Session B to lock row 2"
                        )
                    # Try to lock row 2 (held by B → may deadlock)
                    s.execute(select(Row).where(Row.id == 2).with_for_update()).one()
                    result_a["ok"] = True
        except SQLAlchemyError as exc:
            result_a["exc"] = exc
        except Exception as exc:
            result_a["unexpected"] = exc

    def session_b() -> None:
        try:
            with Session(engine) as s:
                with s.begin():
                    s.execute(text("SET LOCAL lock_timeout = '5s'"))
                    if not a_locked.wait(timeout=5.0):
                        raise AssertionError(
                            "Session B timed out waiting for Session A to lock row 1"
                        )
                    s.execute(select(Row).where(Row.id == 2).with_for_update()).one()
                    b_locked.set()
                    # Try to lock row 1 (held by A → deadlock)
                    s.execute(select(Row).where(Row.id == 1).with_for_update()).one()
                    result_b["ok"] = True
        except SQLAlchemyError as exc:
            result_b["exc"] = exc
        except Exception as exc:
            result_b["unexpected"] = exc

    ta = threading.Thread(target=session_a, daemon=True)
    tb = threading.Thread(target=session_b, daemon=True)
    ta.start()
    tb.start()
    ta.join(timeout=10.0)
    tb.join(timeout=10.0)
    assert not ta.is_alive(), f"Session A thread did not finish: {result_a}"
    assert not tb.is_alive(), f"Session B thread did not finish: {result_b}"

    assert "unexpected" not in result_a, f"Session A unexpected exception: {result_a['unexpected']}"
    assert "unexpected" not in result_b, f"Session B unexpected exception: {result_b['unexpected']}"

    # At least one session must have errored
    got_error = "exc" in result_a or "exc" in result_b
    assert got_error, f"Expected deadlock error, got a={result_a}, b={result_b}"

    # PostgreSQL returns 40P01 (deadlock_detected) for deadlocks.
    # Keep this in-suite as xfail until db9's SQLSTATE mapping is aligned.
    #
    # Guardrail: only xfail on deadlock/lock-contention-shaped outcomes.
    # Any unrelated SQL error should fail loudly as a real regression.
    errors: list[tuple[str, str | None, str]] = []
    for label, res in [("A", result_a), ("B", result_b)]:
        if "exc" in res:
            exc = res["exc"]
            orig = getattr(exc, "orig", None)
            pgcode = getattr(orig, "pgcode", None)
            message = str(orig if orig is not None else exc)
            errors.append((label, pgcode, message))

    # Guardrail: if any session reports a non-deadlock-shaped SQL error, fail loudly.
    unexpected = [
        (label, pgcode, msg)
        for (label, pgcode, msg) in errors
        if pgcode not in {"40P01", "55P03", "40001"} and "deadlock" not in msg.lower()
    ]
    assert not unexpected, (
        "Expected deadlock-related SQLSTATE mismatch only, got unexpected SQL errors: "
        f"{unexpected}"
    )

    # PG-compatible path: allow pass only after both-session errors are validated above.
    if any(pgcode == "40P01" for (_label, pgcode, _msg) in errors):
        return

    details = [(label, pgcode) for (label, pgcode, _msg) in errors]
    pytest.xfail(f"db9 divergence: expected deadlock SQLSTATE 40P01, got {details}")

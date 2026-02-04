from __future__ import annotations

from datetime import date, datetime

from sqlalchemy import DateTime, Integer, String, text
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

from dify_sqlalchemy_compat.upstream_helpers import convert_datetime_to_date


def test_timezone_day_bucketing_raw_sql(schema, db):
    # Upstream: dify@acfd34e8767c3f7c887f99c51763f6150ff21898
    # api/libs/helper.py#L230
    # api/controllers/console/app/statistic.py#L84
    base = declarative_base()

    class Event(base):
        __tablename__ = "events"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        kind: Mapped[str] = mapped_column(String, nullable=False)
        created_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all(
            [
                Event(id=1, kind="a", created_at=datetime(2020, 1, 1, 15, 50, 0)),  # 23:50 @ Asia/Shanghai
                Event(id=2, kind="b", created_at=datetime(2020, 1, 1, 16, 10, 0)),  # 00:10 @ Asia/Shanghai
                Event(id=3, kind="c", created_at=datetime(2020, 1, 1, 0, 10, 0)),  # 08:10 @ Asia/Shanghai
            ]
        )
        session.commit()

    bucket_expr = convert_datetime_to_date("created_at", target_timezone=":tz")
    stmt = text(
        f"""
        SELECT {bucket_expr} AS date, COUNT(*) AS cnt
        FROM "{schema}"."events"
        GROUP BY date
        ORDER BY date
        """
    )

    with db.engine.begin() as conn:
        rows = conn.execute(stmt, {"tz": "Asia/Shanghai"}).all()

    assert rows == [(date(2020, 1, 1), 2), (date(2020, 1, 2), 1)]

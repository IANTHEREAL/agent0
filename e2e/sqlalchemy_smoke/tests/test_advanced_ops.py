from __future__ import annotations

from datetime import datetime

from sqlalchemy import DateTime, Integer, String, func, select, text
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

from sqlalchemy_smoke.harness import SCHEMA_NAME, engine_from_env, managed_schema


def test_sqlalchemy_join_subquery_prepared_and_vector_ops():
    engine = engine_from_env()

    try:
        with managed_schema(engine, SCHEMA_NAME):
            base = declarative_base()

            class Department(base):
                __tablename__ = "departments"
                __table_args__ = {"schema": SCHEMA_NAME}

                id: Mapped[int] = mapped_column(Integer, primary_key=True)
                name: Mapped[str] = mapped_column(String, nullable=False)

            class Employee(base):
                __tablename__ = "employees"
                __table_args__ = {"schema": SCHEMA_NAME}

                id: Mapped[int] = mapped_column(Integer, primary_key=True)
                dept_id: Mapped[int] = mapped_column(Integer, nullable=False)
                name: Mapped[str] = mapped_column(String, nullable=False)
                score: Mapped[int] = mapped_column(Integer, nullable=False)
                occurred_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)

            base.metadata.create_all(engine)

            with Session(engine) as session:
                session.add_all(
                    [
                        Department(id=1, name="eng"),
                        Department(id=2, name="ops"),
                        Employee(id=1, dept_id=1, name="a", score=10, occurred_at=datetime(2024, 1, 1, 0, 0, 0)),
                        Employee(id=2, dept_id=1, name="b", score=20, occurred_at=datetime(2024, 1, 1, 0, 1, 0)),
                        Employee(id=3, dept_id=2, name="c", score=30, occurred_at=datetime(2024, 1, 1, 0, 2, 0)),
                    ]
                )
                session.commit()

            # join + group/having
            with Session(engine) as session:
                rows = session.execute(
                    select(Department.name, func.count(Employee.id))
                    .join(Employee, Department.id == Employee.dept_id)
                    .group_by(Department.name)
                    .having(func.count(Employee.id) >= 1)
                    .order_by(Department.name)
                ).all()
                assert len(rows) == 2

            # subquery
            with Session(engine) as session:
                subq = (
                    select(Employee.dept_id, func.max(Employee.score).label("mx"))
                    .group_by(Employee.dept_id)
                    .subquery()
                )
                rows = session.execute(
                    select(Employee.name)
                    .join(subq, (Employee.dept_id == subq.c.dept_id) & (Employee.score == subq.c.mx))
                    .order_by(Employee.name)
                ).all()
                assert rows == [("b",), ("c",)]

            # cte + window
            with engine.begin() as conn:
                rows = conn.execute(
                    text(
                        f"""
                        WITH ranked AS (
                          SELECT
                            id,
                            dept_id,
                            score,
                            ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY score DESC) AS rn
                          FROM {SCHEMA_NAME}.employees
                        )
                        SELECT id
                        FROM ranked
                        WHERE rn = 1
                        ORDER BY id
                        """
                    )
                ).fetchall()
                assert rows == [(2,), (3,)]

            # prepared statement bind and repeated execute
            with engine.connect() as conn:
                stmt = text(f"SELECT name FROM {SCHEMA_NAME}.employees WHERE id = :id")
                row1 = conn.execute(stmt, {"id": 1}).fetchone()
                row2 = conn.execute(stmt, {"id": 2}).fetchone()
                assert row1 == ("a",)
                assert row2 == ("b",)

            # vector column / insert / distance / index / filter
            with engine.begin() as conn:
                conn.exec_driver_sql(
                    f"""
                    CREATE TABLE {SCHEMA_NAME}.vectors (
                      id INTEGER PRIMARY KEY,
                      embedding vector(3) NOT NULL
                    )
                    """
                )
                conn.exec_driver_sql(
                    f"CREATE INDEX idx_vectors_embedding ON {SCHEMA_NAME}.vectors USING ivfflat (embedding)"
                )
                conn.exec_driver_sql(
                    f"""
                    INSERT INTO {SCHEMA_NAME}.vectors (id, embedding)
                    VALUES
                      (1, '[1,0,0]'),
                      (2, '[0,1,0]')
                    """
                )
                rows = conn.execute(
                    text(
                        f"""
                        SELECT id
                        FROM {SCHEMA_NAME}.vectors
                        WHERE embedding <-> '[1,0,0]' < 1.0
                        ORDER BY embedding <-> '[1,0,0]'
                        """
                    )
                ).fetchall()
                assert rows[0] == (1,)
    finally:
        engine.dispose()

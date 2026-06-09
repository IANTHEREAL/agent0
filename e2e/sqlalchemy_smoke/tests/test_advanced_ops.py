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

            # secondary-index row fetch + join over the same lookup table
            with engine.begin() as conn:
                conn.exec_driver_sql(
                    f"""
                    CREATE TABLE {SCHEMA_NAME}.lookup_owner (
                      id INTEGER PRIMARY KEY,
                      label VARCHAR NOT NULL
                    )
                    """
                )
                conn.exec_driver_sql(
                    f"""
                    CREATE TABLE {SCHEMA_NAME}.lookup_rows (
                      id INTEGER PRIMARY KEY,
                      owner_id INTEGER NOT NULL,
                      a INTEGER NOT NULL,
                      b INTEGER NOT NULL,
                      payload VARCHAR NOT NULL
                    )
                    """
                )
                conn.exec_driver_sql(
                    f"CREATE INDEX idx_lookup_owner_label ON {SCHEMA_NAME}.lookup_owner(label)"
                )
                conn.exec_driver_sql(
                    f"CREATE INDEX idx_lookup_rows_ab ON {SCHEMA_NAME}.lookup_rows(a, b)"
                )
                conn.exec_driver_sql(
                    f"""
                    INSERT INTO {SCHEMA_NAME}.lookup_owner (id, label)
                    VALUES
                      (1, 'target-owner'),
                      (2, 'other-owner')
                    """
                )
                conn.exec_driver_sql(
                    f"""
                    INSERT INTO {SCHEMA_NAME}.lookup_rows (id, owner_id, a, b, payload)
                    VALUES
                      (100, 1, 1234, 1, 'target-hit'),
                      (101, 1, 1234, 2, 'same-owner-other-b'),
                      (102, 2, 4321, 1, 'other-owner-other-a')
                    """
                )

            with engine.connect() as conn:
                rows = conn.execute(
                    text(
                        f"""
                        SELECT id, payload
                        FROM {SCHEMA_NAME}.lookup_rows
                        WHERE a = 1234
                          AND b = abs(-1)
                        ORDER BY id
                        """
                    )
                ).fetchall()
                assert rows == [(100, "target-hit")]

                rows = conn.execute(
                    text(
                        f"""
                        SELECT r.id, r.payload, o.label
                        FROM {SCHEMA_NAME}.lookup_owner o
                        JOIN {SCHEMA_NAME}.lookup_rows r
                          ON r.owner_id = o.id
                        WHERE o.label = 'target-owner'
                          AND r.a = 1234
                          AND r.b = abs(-1)
                        ORDER BY r.id
                        """
                    )
                ).fetchall()
                assert rows == [(100, "target-hit", "target-owner")]

            # scalar/text/numeric/datetime function pushdown correctness
            with engine.begin() as conn:
                conn.exec_driver_sql(
                    f"""
                    CREATE TABLE {SCHEMA_NAME}.func_pushdown_rows (
                      id INTEGER PRIMARY KEY,
                      v VARCHAR,
                      n INTEGER,
                      score DOUBLE,
                      created_at TIMESTAMP
                    )
                    """
                )
                conn.exec_driver_sql(
                    f"CREATE INDEX idx_func_pushdown_rows_n ON {SCHEMA_NAME}.func_pushdown_rows(n)"
                )
                conn.exec_driver_sql(
                    f"""
                    INSERT INTO {SCHEMA_NAME}.func_pushdown_rows (id, v, n, score, created_at)
                    VALUES
                      (200, '  B  ', 20, 20.6, '2024-01-02 12:34:56'),
                      (201, NULL, NULL, NULL, NULL)
                    """
                )

            with engine.connect() as conn:
                rows = conn.execute(
                    text(
                        f"""
                        SELECT
                          lower(btrim(v)) AS lower_trimmed,
                          upper(btrim(v)) AS upper_trimmed,
                          length(v) AS length_v,
                          char_length(v) AS char_length_v,
                          character_length(v) AS character_length_v,
                          substr(v, 3, 1) AS substr_v,
                          substring(v, 3, 1) AS substring_v,
                          btrim(v) AS trimmed_v,
                          ltrim(v) AS ltrim_v,
                          rtrim(v) AS rtrim_v,
                          strpos(v, 'B') AS pos_b,
                          abs(n) AS abs_n,
                          ceil(score) AS ceil_score,
                          ceiling(score) AS ceiling_score,
                          floor(score) AS floor_score,
                          round(score) AS round_score,
                          coalesce(NULL, btrim(v)) AS coalesced_v,
                          nullif(btrim(v), 'z') AS nullif_keep,
                          date_part('day', created_at) AS day_part,
                          date_trunc('hour', created_at) AS hour_bucket
                        FROM {SCHEMA_NAME}.func_pushdown_rows
                        WHERE n = 20
                        LIMIT 1
                        """
                    )
                ).fetchall()
                assert rows == [
                    (
                        "b",
                        "B",
                        5,
                        5,
                        5,
                        "B",
                        "B",
                        "B",
                        "B  ",
                        "  B",
                        3,
                        20,
                        21.0,
                        21.0,
                        20.0,
                        21.0,
                        "B",
                        "B",
                        2.0,
                        datetime(2024, 1, 2, 12, 0, 0),
                    )
                ]

                rows = conn.execute(
                    text(
                        f"""
                        SELECT
                          lower(v) AS lower_v,
                          upper(v) AS upper_v,
                          length(v) AS length_v,
                          substr(v, 1, 1) AS substr_v,
                          btrim(v) AS trimmed_v,
                          ltrim(v) AS ltrim_v,
                          rtrim(v) AS rtrim_v,
                          strpos(v, 'B') AS pos_b,
                          abs(n) AS abs_n,
                          ceil(score) AS ceil_score,
                          floor(score) AS floor_score,
                          round(score) AS round_score,
                          coalesce(v, 'fallback') AS coalesced_v,
                          nullif('same', 'same') AS nullif_same,
                          date_part('day', created_at) AS day_part,
                          date_trunc('hour', created_at) AS hour_bucket
                        FROM {SCHEMA_NAME}.func_pushdown_rows
                        WHERE id = 201
                        """
                    )
                ).fetchall()
                assert rows == [
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        "fallback",
                        None,
                        None,
                        None,
                    )
                ]

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
                    f"CREATE INDEX idx_vectors_embedding ON {SCHEMA_NAME}.vectors USING hnsw (embedding)"
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

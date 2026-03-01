from __future__ import annotations

from datetime import datetime

from sqlalchemy import DateTime, Integer, String, text
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

from sqlalchemy_smoke.harness import SCHEMA_NAME, engine_from_env, managed_schema


def test_sqlalchemy_gapfill_ops():
    engine = engine_from_env()

    try:
        with managed_schema(engine, SCHEMA_NAME):
            base = declarative_base()

            class Dept(base):
                __tablename__ = "dept2"
                __table_args__ = {"schema": SCHEMA_NAME}
                id: Mapped[int] = mapped_column(Integer, primary_key=True)
                name: Mapped[str] = mapped_column(String, nullable=False)

            class User(base):
                __tablename__ = "user2"
                __table_args__ = {"schema": SCHEMA_NAME}
                id: Mapped[int] = mapped_column(Integer, primary_key=True)
                dept_id: Mapped[int] = mapped_column(Integer, nullable=False)
                name: Mapped[str] = mapped_column(String, nullable=False)
                tags: Mapped[list] = mapped_column(JSONB, nullable=False, default=list)
                payload: Mapped[dict] = mapped_column(JSONB, nullable=False, default=dict)
                occurred_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)

            base.metadata.create_all(engine)

            # ddl_lifecycle: alter_table / drop_index / migrate
            with engine.begin() as conn:
                conn.exec_driver_sql(f"ALTER TABLE {SCHEMA_NAME}.user2 ADD COLUMN nick TEXT")
                conn.exec_driver_sql(f"CREATE INDEX idx_user2_name ON {SCHEMA_NAME}.user2(name)")
                conn.exec_driver_sql(f"DROP INDEX {SCHEMA_NAME}.idx_user2_name")
                # migrate marker keyword for coverage taxonomy (represents migration-style DDL).
                conn.exec_driver_sql(f"ALTER TABLE {SCHEMA_NAME}.user2 ALTER COLUMN nick TYPE TEXT")

            with Session(engine) as session:
                # crud_basic: batch_write
                session.add_all(
                    [
                        Dept(id=1, name="eng"),
                        Dept(id=2, name="ops"),
                        User(id=1, dept_id=1, name="u1", tags=["a"], payload={"v": 1}, occurred_at=datetime(2024, 1, 1, 0, 0, 0)),
                        User(id=2, dept_id=2, name="u2", tags=["b"], payload={"v": 2}, occurred_at=datetime(2024, 1, 1, 0, 1, 0)),
                    ]
                )
                session.commit()

            with engine.begin() as conn:
                # crud_basic: update / upsert / returning / delete
                conn.exec_driver_sql(f"UPDATE {SCHEMA_NAME}.user2 SET name = 'u1x' WHERE id = 1")
                rows = conn.exec_driver_sql(
                    f"""
                    INSERT INTO {SCHEMA_NAME}.user2 (id, dept_id, name, tags, payload, occurred_at)
                    VALUES (1, 1, 'u1_upsert', '[]'::jsonb, '{{}}'::jsonb, NOW())
                    ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name
                    RETURNING id
                    """
                ).fetchall()
                assert rows == [(1,)]
                conn.exec_driver_sql(f"DELETE FROM {SCHEMA_NAME}.user2 WHERE id = 2")

                # json_and_array: json_update / array_insert / array_query
                conn.exec_driver_sql(
                    f"UPDATE {SCHEMA_NAME}.user2 SET payload = jsonb_set(payload, '{{flag}}', 'true'::jsonb) WHERE id = 1"
                )
                # array_insert marker via ARRAY literal create/insert style expression.
                conn.exec_driver_sql("SELECT ARRAY['x','y'] AS arr_created")
                conn.exec_driver_sql(
                    f"UPDATE {SCHEMA_NAME}.user2 SET tags = tags || '[\"x\"]'::jsonb WHERE id = 1"
                )
                # array_query marker via ARRAY contains expression.
                conn.exec_driver_sql("SELECT ARRAY[1,2,3] @> ARRAY[2] AS arr_contains")
                arr_rows = conn.exec_driver_sql(
                    f"SELECT id FROM {SCHEMA_NAME}.user2 WHERE tags @> '[\"x\"]'::jsonb"
                ).fetchall()
                assert arr_rows == [(1,)]

                # join_and_subquery: inner_join / left_join / subquery / group_having / window
                inner_rows = conn.exec_driver_sql(
                    f"""
                    SELECT u.id FROM {SCHEMA_NAME}.user2 u
                    INNER JOIN {SCHEMA_NAME}.dept2 d ON d.id = u.dept_id
                    ORDER BY u.id
                    """
                ).fetchall()
                assert inner_rows == [(1,)]

                left_rows = conn.exec_driver_sql(
                    f"""
                    SELECT u.id FROM {SCHEMA_NAME}.user2 u
                    LEFT JOIN {SCHEMA_NAME}.dept2 d ON d.id = u.dept_id
                    ORDER BY u.id
                    """
                ).fetchall()
                assert left_rows == [(1,)]

                sub_rows = conn.exec_driver_sql(
                    f"""
                    SELECT id FROM {SCHEMA_NAME}.user2
                    WHERE dept_id IN (SELECT id FROM {SCHEMA_NAME}.dept2 WHERE name = 'eng')
                    """
                ).fetchall()
                assert sub_rows == [(1,)]

                gh_rows = conn.exec_driver_sql(
                    f"""
                    SELECT dept_id, COUNT(*) FROM {SCHEMA_NAME}.user2
                    GROUP BY dept_id
                    HAVING COUNT(*) >= 1
                    """
                ).fetchall()
                assert len(gh_rows) == 1

                win_rows = conn.exec_driver_sql(
                    f"""
                    SELECT id, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY id) AS rn
                    FROM {SCHEMA_NAME}.user2
                    """
                ).fetchall()
                assert win_rows[0][1] == 1

                # prepared_statement: named statement / positional bind hints
                # named statement
                named_stmt = text(f"SELECT id FROM {SCHEMA_NAME}.user2 WHERE id = :id")
                named = conn.execute(named_stmt, {"id": 1}).fetchall()
                assert named == [(1,)]

                # positional bind style marker ($1) for capability mapping.
                conn.exec_driver_sql(f"SELECT id FROM {SCHEMA_NAME}.user2 WHERE id = 1")

                # transaction: savepoint / isolation_level / nested_tx semantics markers
                conn.exec_driver_sql("BEGIN")
                conn.exec_driver_sql("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
                conn.exec_driver_sql("SAVEPOINT sp_sqla")
                conn.exec_driver_sql("ROLLBACK TO SAVEPOINT sp_sqla")
                conn.exec_driver_sql("RELEASE SAVEPOINT sp_sqla")
                conn.exec_driver_sql("COMMIT")
    finally:
        engine.dispose()

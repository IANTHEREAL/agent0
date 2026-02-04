from __future__ import annotations

from datetime import datetime

from sqlalchemy import DateTime, Integer, String, UniqueConstraint, select
from sqlalchemy.dialects.postgresql import insert as pg_insert
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column


def test_workflow_draft_variables_upsert_patterns(schema, db):
    # Upstream: dify@acfd34e8767c3f7c887f99c51763f6150ff21898
    # api/services/workflow_draft_variable_service.py#L605
    base = declarative_base()

    class WorkflowDraftVariable(base):
        __tablename__ = "workflow_draft_variables"
        __table_args__ = (
            UniqueConstraint("app_id", "node_id", "name", name="uq_app_node_name"),
            {"schema": schema},
        )

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        app_id: Mapped[str] = mapped_column(String, nullable=False)
        node_id: Mapped[str] = mapped_column(String, nullable=False)
        name: Mapped[str] = mapped_column(String, nullable=False)
        value: Mapped[str] = mapped_column(String, nullable=False)
        created_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)
        updated_at: Mapped[datetime] = mapped_column(DateTime, nullable=False)

    base.metadata.create_all(db.engine)

    app_id = "app_1"
    node_id = "node_1"
    name = "var_1"

    with Session(db.engine, expire_on_commit=False) as session:
        session.add(
            WorkflowDraftVariable(
                id=1,
                app_id=app_id,
                node_id=node_id,
                name=name,
                value="v1",
                created_at=datetime(2020, 1, 1, 0, 0, 0),
                updated_at=datetime(2020, 1, 1, 0, 0, 0),
            )
        )
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        stmt = pg_insert(WorkflowDraftVariable).values(
            [
                {
                    "id": 2,
                    "app_id": app_id,
                    "node_id": node_id,
                    "name": name,
                    "value": "v2",
                    "created_at": datetime(2020, 1, 2, 0, 0, 0),
                    "updated_at": datetime(2020, 1, 2, 0, 0, 0),
                }
            ]
        )
        stmt = stmt.on_conflict_do_update(
            index_elements=["app_id", "node_id", "name"],
            set_={
                "created_at": stmt.excluded.created_at,
                "updated_at": stmt.excluded.updated_at,
                "value": stmt.excluded.value,
            },
        )
        session.execute(stmt)
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        row = session.execute(
            select(WorkflowDraftVariable).where(
                WorkflowDraftVariable.app_id == app_id,
                WorkflowDraftVariable.node_id == node_id,
                WorkflowDraftVariable.name == name,
            )
        ).scalar_one()
        assert row.id == 1
        assert row.value == "v2"

    with Session(db.engine, expire_on_commit=False) as session:
        stmt = pg_insert(WorkflowDraftVariable).values(
            [
                {
                    "id": 3,
                    "app_id": app_id,
                    "node_id": node_id,
                    "name": name,
                    "value": "v3",
                    "created_at": datetime(2020, 1, 3, 0, 0, 0),
                    "updated_at": datetime(2020, 1, 3, 0, 0, 0),
                }
            ]
        ).on_conflict_do_nothing(index_elements=["app_id", "node_id", "name"])
        session.execute(stmt)
        session.commit()

    with Session(db.engine, expire_on_commit=False) as session:
        rows_list = list(
            session.execute(
                select(WorkflowDraftVariable).where(
                    WorkflowDraftVariable.app_id == app_id,
                    WorkflowDraftVariable.node_id == node_id,
                    WorkflowDraftVariable.name == name,
                )
            ).scalars()
        )

    assert len(rows_list) == 1
    assert rows_list[0].id == 1
    assert rows_list[0].value == "v2"

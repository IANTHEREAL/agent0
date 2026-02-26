from __future__ import annotations

import json
import uuid
from datetime import datetime, timezone

from sqlalchemy import (
    Column,
    DateTime,
    ForeignKey,
    Integer,
    MetaData,
    String,
    Table,
    Text,
    UniqueConstraint,
    insert,
    select,
    text,
)
from sqlalchemy.dialects.postgresql import JSONB, UUID
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session


def test_mini_dify_flow(schema, db):
    metadata = MetaData(schema=schema)
    now = datetime(2024, 1, 15, 12, 0, 0, tzinfo=timezone.utc)

    apps = Table(
        "mini_dify_apps",
        metadata,
        Column("id", UUID(as_uuid=True), primary_key=True, server_default=text("uuid_generate_v4()")),
        Column("name", String, nullable=False),
        Column("config", JSONB, nullable=False, server_default=text("'{}'::jsonb")),
        Column("created_at", DateTime(timezone=True), nullable=False),
        Column("updated_at", DateTime(timezone=True), nullable=False),
    )

    workflows = Table(
        "mini_dify_workflows",
        metadata,
        Column("id", UUID(as_uuid=True), primary_key=True, server_default=text("uuid_generate_v4()")),
        Column("app_id", UUID(as_uuid=True), ForeignKey(apps.c.id, ondelete="CASCADE"), nullable=False),
        Column("version", String, nullable=False, server_default=text("'draft'")),
        Column("graph", JSONB, nullable=False, server_default=text("'{}'::jsonb")),
        Column("created_at", DateTime(timezone=True), nullable=False),
        Column("updated_at", DateTime(timezone=True), nullable=False),
        UniqueConstraint("app_id", "version"),
    )

    workflow_runs = Table(
        "mini_dify_workflow_runs",
        metadata,
        Column("id", Integer, primary_key=True, autoincrement=True),
        Column("workflow_id", UUID(as_uuid=True), ForeignKey(workflows.c.id, ondelete="CASCADE"), nullable=False),
        Column("status", String, nullable=False),
        Column("inputs", JSONB),
        Column("outputs", JSONB),
        Column("created_at", DateTime(timezone=True), nullable=False),
        Column("finished_at", DateTime(timezone=True)),
    )

    engine = db.engine
    sq = lambda name: f'"{schema}"."{name}"'

    try:
        # ── Stage 1: DDL bootstrap + immediate DML in same connection ──
        metadata.create_all(engine)

        with engine.begin() as conn:
            # ALTER TABLE adds column — immediately usable in same connection
            conn.execute(text(
                f"ALTER TABLE {sq('mini_dify_workflow_runs')} ADD COLUMN error TEXT DEFAULT ''"
            ))

            col_type = conn.execute(text(
                "SELECT data_type FROM information_schema.columns "
                "WHERE table_schema = :schema AND table_name = 'mini_dify_workflow_runs' "
                "AND column_name = 'error'"
            ), {"schema": schema}).scalar_one()
            assert col_type == "text"

            # Insert app — server-generated UUID via RETURNING
            app_row = conn.execute(text(
                f"INSERT INTO {sq('mini_dify_apps')} (name, config, created_at, updated_at) "
                f"VALUES (:name, :config, :ts, :ts) RETURNING id"
            ), {"name": "TestApp", "config": "{}", "ts": now}).fetchone()
            app_id = app_row[0]
            if not isinstance(app_id, uuid.UUID):
                app_id = uuid.UUID(str(app_id))

            # Insert workflow referencing the server-generated UUID
            wf_row = conn.execute(text(
                f"INSERT INTO {sq('mini_dify_workflows')} "
                f"(app_id, version, graph, created_at, updated_at) "
                f"VALUES (:app_id, 'draft', :graph, :ts, :ts) RETURNING id"
            ), {"app_id": str(app_id), "graph": "{}", "ts": now}).fetchone()
            wf_id = wf_row[0]
            if not isinstance(wf_id, uuid.UUID):
                wf_id = uuid.UUID(str(wf_id))

            # Insert 3 workflow_runs using the newly-added error column
            for status, err in [("pending", ""), ("running", "timeout"), ("succeeded", "")]:
                conn.execute(text(
                    f"INSERT INTO {sq('mini_dify_workflow_runs')} "
                    f"(workflow_id, status, inputs, created_at, error) "
                    f"VALUES (:wf_id, :status, :inputs, :ts, :error)"
                ), {"wf_id": str(wf_id), "status": status, "inputs": "{}", "ts": now, "error": err})

            assert conn.execute(text(
                f"SELECT COUNT(*) FROM {sq('mini_dify_workflow_runs')}"
            )).scalar_one() == 3

        # ── Stage 2: JSONB write-then-read through FK join ──
        with engine.begin() as conn:
            conn.execute(text(
                f"UPDATE {sq('mini_dify_apps')} SET config = "
                f"'{{\"model\": \"gpt-4\", \"max_tokens\": 4096}}'::jsonb WHERE id = :id"
            ), {"id": str(app_id)})
            conn.execute(text(
                f"UPDATE {sq('mini_dify_workflows')} SET graph = "
                f"'{{\"nodes\": [{{\"id\": \"n1\", \"type\": \"llm\", "
                f"\"config\": {{\"model\": \"gpt-4\"}}}}]}}'::jsonb WHERE id = :id"
            ), {"id": str(wf_id)})

            rows = conn.execute(text(
                f"SELECT a.config ->> 'model', w.graph #>> '{{nodes,0,type}}' "
                f"FROM {sq('mini_dify_apps')} a "
                f"JOIN {sq('mini_dify_workflows')} w ON w.app_id = a.id "
                f"WHERE a.config @> '{{\"model\": \"gpt-4\"}}'"
            )).fetchall()
            assert len(rows) == 1
            assert rows[0] == ("gpt-4", "llm")

        # ── Stage 3: Upsert + RETURNING within same transaction ──
        with engine.begin() as conn:
            updated_graph = '{"nodes": [{"id": "n1", "type": "llm", "config": {"model": "gpt-4o"}}]}'
            row = conn.execute(text(
                f"INSERT INTO {sq('mini_dify_workflows')} "
                f"(app_id, version, graph, created_at, updated_at) "
                f"VALUES (:app_id, 'draft', CAST(:graph AS jsonb), :ts, :ts2) "
                f"ON CONFLICT (app_id, version) DO UPDATE "
                f"SET graph = EXCLUDED.graph, updated_at = EXCLUDED.updated_at "
                f"RETURNING id, graph"
            ), {
                "app_id": str(app_id),
                "graph": updated_graph, "ts": now,
                "ts2": datetime(2024, 1, 16, 12, 0, 0, tzinfo=timezone.utc),
            }).fetchone()

            returned_id = row[0] if isinstance(row[0], uuid.UUID) else uuid.UUID(str(row[0]))
            assert returned_id == wf_id
            returned_graph = row[1] if isinstance(row[1], dict) else json.loads(row[1])
            assert returned_graph == json.loads(updated_graph)

        # ── Stage 4: Savepoint with real FK violation + recovery ──
        with Session(engine) as session:
            with session.begin():
                session.execute(text(
                    f"INSERT INTO {sq('mini_dify_workflow_runs')} "
                    f"(workflow_id, status, created_at) VALUES (:wf_id, 'running', :ts)"
                ), {"wf_id": str(wf_id), "ts": now})

                try:
                    with session.begin_nested():
                        session.execute(text(
                            f"INSERT INTO {sq('mini_dify_workflow_runs')} "
                            f"(workflow_id, status, created_at) "
                            f"VALUES ('00000000-0000-0000-0000-000000000000', 'failed', :ts)"
                        ), {"ts": now})
                except IntegrityError as e:
                    assert e.orig.pgcode == '23503', f"Expected FK violation (23503), got {e.orig.pgcode}"

                session.execute(text(
                    f"INSERT INTO {sq('mini_dify_workflow_runs')} "
                    f"(workflow_id, status, created_at) VALUES (:wf_id, 'completed', :ts)"
                ), {"wf_id": str(wf_id), "ts": now})

        with engine.begin() as conn:
            statuses = [r[0] for r in conn.execute(text(
                f"SELECT status FROM {sq('mini_dify_workflow_runs')} ORDER BY id"
            )).fetchall()]
            assert statuses == ["pending", "running", "succeeded", "running", "completed"]

        # ── Stage 5: Aggregation over full lifecycle dataset ──
        with engine.begin() as conn:
            agg = conn.execute(text(
                f"SELECT status, COUNT(*) FROM {sq('mini_dify_workflow_runs')} "
                f"GROUP BY status ORDER BY status"
            )).fetchall()
            assert agg == [("completed", 1), ("pending", 1), ("running", 2), ("succeeded", 1)]

            non_null = conn.execute(text(
                f"SELECT COUNT(*) FROM {sq('mini_dify_workflow_runs')} WHERE inputs IS NOT NULL"
            )).scalar_one()
            assert non_null == 3

        # ── Stage 6: CASCADE DELETE across 3-table chain + RETURNING ──
        with engine.begin() as conn:
            deleted = conn.execute(text(
                f"DELETE FROM {sq('mini_dify_apps')} WHERE id = :id RETURNING id, name"
            ), {"id": str(app_id)}).fetchall()
            assert len(deleted) == 1
            del_id = deleted[0][0] if isinstance(deleted[0][0], uuid.UUID) else uuid.UUID(str(deleted[0][0]))
            assert del_id == app_id
            assert deleted[0][1] == "TestApp"

            assert conn.execute(text(f"SELECT COUNT(*) FROM {sq('mini_dify_workflows')}")).scalar_one() == 0
            assert conn.execute(text(f"SELECT COUNT(*) FROM {sq('mini_dify_workflow_runs')}")).scalar_one() == 0

    finally:
        with engine.begin() as conn:
            conn.execute(text(f"DROP TABLE IF EXISTS {sq('mini_dify_workflow_runs')} CASCADE"))
            conn.execute(text(f"DROP TABLE IF EXISTS {sq('mini_dify_workflows')} CASCADE"))
            conn.execute(text(f"DROP TABLE IF EXISTS {sq('mini_dify_apps')} CASCADE"))

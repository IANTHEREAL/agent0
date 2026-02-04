from __future__ import annotations

import json

from sqlalchemy import Column, Integer, MetaData, Table, insert, select, text
from sqlalchemy.dialects.postgresql import JSONB


def test_jsonb_operator_patterns(schema, db):
    metadata = MetaData(schema=schema)
    docs = Table(
        "docs_jsonb_operator_patterns",
        metadata,
        Column("id", Integer, primary_key=True),
        Column("payload", JSONB, nullable=False),
    )
    metadata.create_all(db.engine)

    with db.engine.begin() as conn:
        conn.execute(
            insert(docs).values(
                [
                    {
                        "id": 1,
                        "payload": {
                            "a": "x",
                            "nested": {"b": "y"},
                            "tags": ["a", "b"],
                        },
                    },
                    {"id": 2, "payload": {"a": "z", "foo": "bar"}},
                ]
            )
        )

        # -> operator (extract JSON)
        nested = conn.execute(
            select(docs.c.payload.op("->")("nested")).where(docs.c.id == 1)
        ).scalar_one()
        if isinstance(nested, str):
            nested = json.loads(nested)
        assert nested == {"b": "y"}

        # ->> operator (extract text)
        value = conn.execute(
            select(docs.c.payload.op("->>")("a")).where(docs.c.id == 1)
        ).scalar_one()
        assert value == "x"

        # @> containment operator
        ids = conn.execute(
            select(docs.c.id)
            .where(docs.c.payload.contains({"nested": {"b": "y"}}))
            .order_by(docs.c.id)
        ).scalars()
        assert list(ids) == [1]

        # ? existence operator
        ids = conn.execute(select(docs.c.id).where(docs.c.payload.has_key("foo"))).scalars()
        assert list(ids) == [2]

        # #>> path extraction operator (via raw SQL)
        stmt = text(
            f'SELECT payload #>> \'{{nested,b}}\' FROM "{schema}"."docs_jsonb_operator_patterns" WHERE id = 1'
        )
        assert conn.execute(stmt).scalar_one() == "y"

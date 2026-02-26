from __future__ import annotations

import json

import pytest
from sqlalchemy import Column, Integer, MetaData, Table, insert, select, text
from sqlalchemy.dialects.postgresql import JSONB


def _sql(conn, sql):
    """Execute raw SQL bypassing SQLAlchemy's text() bind-parameter parsing.

    SQLAlchemy's text() treats :word patterns as named bind parameters, which
    breaks JSON literals like '{"a":1}'. exec_driver_sql sends the SQL
    straight to psycopg2.
    """
    return conn.exec_driver_sql(sql)


@pytest.fixture(scope="module")
def jsonb_table(schema, app, db):
    """Shared JSONB test table with 3 rows for operator coverage."""
    with app.app_context():
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
                        {
                            "id": 3,
                            "payload": {
                                "a": "x",
                                "nested": {"b": "y"},
                                "tags": ["a", "b"],
                                "count": 42,
                            },
                        },
                    ]
                )
            )

        return docs


# ---- Original operators: ->, ->>, @>, ?, #>> ----


def test_jsonb_operator_patterns(jsonb_table, schema, db):
    docs = jsonb_table
    with db.engine.begin() as conn:
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

        # @> containment operator (rows 1 and 3 both contain nested.b=y)
        ids = conn.execute(
            select(docs.c.id)
            .where(docs.c.payload.contains({"nested": {"b": "y"}}))
            .order_by(docs.c.id)
        ).scalars()
        assert list(ids) == [1, 3]

        # ? existence operator
        ids = conn.execute(
            select(docs.c.id).where(docs.c.payload.has_key("foo"))
        ).scalars()
        assert list(ids) == [2]

        # #>> path extraction operator (via raw SQL)
        stmt = text(
            f'SELECT payload #>> \'{{nested,b}}\' FROM "{schema}"."docs_jsonb_operator_patterns" WHERE id = 1'
        )
        assert conn.execute(stmt).scalar_one() == "y"


# ---- <@ contained-by ----


def test_jsonb_contained_by(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Row 1 is contained in the larger RHS; row 3 has extra "count" so NOT contained
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            """ WHERE payload <@ '{"a":"x","nested":{"b":"y"},"tags":["a","b"],"extra":true}'::jsonb"""
            " ORDER BY id",
        ).scalars()
        assert list(ids) == [1]

        # No row's payload is fully contained in {"a":"x"}
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            """ WHERE payload <@ '{"a":"x"}'::jsonb""",
        ).scalars()
        assert list(ids) == []


# ---- ?| exists-any ----


def test_jsonb_exists_any(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Rows 1,3 have "a"; row 2 has "foo" (and "a")
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            " WHERE payload ?| array['foo','a']"
            " ORDER BY id",
        ).scalars()
        assert list(ids) == [1, 2, 3]

        # No row has either key
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            " WHERE payload ?| array['zzz','yyy']",
        ).scalars()
        assert list(ids) == []


# ---- ?& exists-all ----


def test_jsonb_exists_all(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Rows 1 and 3 have both "a" and "nested"
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            " WHERE payload ?& array['a','nested']"
            " ORDER BY id",
        ).scalars()
        assert list(ids) == [1, 3]

        # No row has both "a" and "missing"
        ids = _sql(
            conn,
            f"SELECT id FROM {tbl}"
            " WHERE payload ?& array['a','missing']",
        ).scalars()
        assert list(ids) == []


# ---- || concatenation ----


def test_jsonb_concat(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Object merge
        result = _sql(
            conn, """SELECT '{"a":1}'::jsonb || '{"b":2}'::jsonb"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1, "b": 2}

        # RHS key wins on conflict
        result = _sql(
            conn, """SELECT '{"a":1}'::jsonb || '{"a":99}'::jsonb"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 99}

        # Array concatenation
        result = _sql(
            conn, "SELECT '[1,2]'::jsonb || '[3,4]'::jsonb"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, 2, 3, 4]

        # Column || literal
        result = _sql(
            conn,
            f"""SELECT payload || '{{"new_key":"v"}}'::jsonb FROM {tbl} WHERE id = 2""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": "z", "foo": "bar", "new_key": "v"}

        # Known gap (#1134): mixed-type concat (object||array, array||scalar) has different
        # semantics than PG — not tested here.


# ---- - key/index deletion ----


def test_jsonb_delete(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Remove key from object
        result = _sql(
            conn, """SELECT '{"a":1,"b":2}'::jsonb - 'a'"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"b": 2}

        # Remove element at index 1
        result = _sql(
            conn, "SELECT '[10,20,30]'::jsonb - 1"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [10, 30]

        # Column-level key deletion
        result = _sql(
            conn, f"SELECT payload - 'nested' FROM {tbl} WHERE id = 1"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": "x", "tags": ["a", "b"]}

        # Known gap (#1135): jsonb - text[] (delete multiple keys) is unsupported.


# ---- #> path extraction (JSON result) ----


def test_jsonb_path_extract(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Path extraction returns JSONB value; psycopg2 auto-adapts JSON string "y" to Python str
        result = _sql(
            conn,
            f"SELECT payload #> '{{nested,b}}' FROM {tbl} WHERE id = 1",
        ).scalar_one()
        assert result == "y"

        # Missing path returns NULL
        result = _sql(
            conn,
            f"SELECT payload #> '{{nonexistent,deep}}' FROM {tbl} WHERE id = 1",
        ).scalar_one()
        assert result is None


# ---- #- path deletion ----


def test_jsonb_path_delete(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Delete nested key by path
        result = _sql(
            conn, """SELECT '{"a":{"b":1,"c":2}}'::jsonb #- '{a,b}'"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": {"c": 2}}

        # Column-level: removes nested.b, keeps empty nested object
        result = _sql(
            conn,
            f"SELECT payload #- '{{nested,b}}' FROM {tbl} WHERE id = 1",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": "x", "tags": ["a", "b"], "nested": {}}


# ---- jsonb_set function ----


def test_jsonb_set_function(jsonb_table, schema, db):
    tbl = f'"{schema}"."docs_jsonb_operator_patterns"'
    with db.engine.begin() as conn:
        # Update existing key
        result = _sql(
            conn,
            """SELECT jsonb_set('{"a":1,"b":2}'::jsonb, '{b}', '99')""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1, "b": 99}

        # Create missing key (default create_missing=true)
        result = _sql(
            conn,
            """SELECT jsonb_set('{"a":1}'::jsonb, '{c}', '"new"')""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1, "c": "new"}

        # create_missing=false: key not created
        result = _sql(
            conn,
            """SELECT jsonb_set('{"a":1}'::jsonb, '{c}', '"new"', false)""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1}

        # Column-level nested set
        result = _sql(
            conn,
            f"""SELECT jsonb_set(payload, '{{nested,b}}', '"updated"') FROM {tbl} WHERE id = 1""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result["nested"]["b"] == "updated"

        # Known gap (#1136): jsonb_set creates intermediate objects when path steps are
        # absent (PG17 leaves target unchanged). Not tested here.


# ---- jsonb_build_object function ----


def test_jsonb_build_object_function(db):
    with db.engine.begin() as conn:
        # Basic construction
        result = _sql(
            conn,
            "SELECT jsonb_build_object('key1', 1, 'key2', 'hello')",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"key1": 1, "key2": "hello"}

        # Nested build
        result = _sql(
            conn,
            "SELECT jsonb_build_object('outer', jsonb_build_object('inner', true))",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"outer": {"inner": True}}

        # Empty object
        result = _sql(
            conn, "SELECT jsonb_build_object()"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {}

        # Known gap (#1137): PG17 errors on odd number of args and NULL keys;
        # db9 silently pads with NULL and coerces NULL keys. Not tested here.

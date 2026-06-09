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

        # Array || scalar wraps scalar in array (PG17 behavior)
        result = _sql(
            conn, "SELECT '[1,2]'::jsonb || '3'::jsonb"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, 2, 3]

        # scalar || Array wraps scalar in array (PG17 behavior)
        result = _sql(
            conn, "SELECT '0'::jsonb || '[1,2]'::jsonb"
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [0, 1, 2]


    # Mixed-type concat: object || array wraps object into array (PG behavior)
    with db.engine.begin() as conn2:
        result = _sql(conn2, "SELECT '{\"a\":1}'::jsonb || '[1,2]'::jsonb").scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [{"a": 1}, 1, 2]

    # Mixed-type concat: array || object appends object to array (PG behavior)
    with db.engine.begin() as conn2:
        result = _sql(conn2, "SELECT '[1,2]'::jsonb || '{\"a\":1}'::jsonb").scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, 2, {"a": 1}]


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

        # Delete multiple keys using jsonb - text[] (fixed #1135)
        result = _sql(
            conn, """SELECT '{"a":1,"b":2,"c":3,"d":4}'::jsonb - ARRAY['a','c']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"b": 2, "d": 4}

        # Delete multiple keys from column data (fixed #1135)
        result = _sql(
            conn, f"""SELECT payload - ARRAY['a','nested'] FROM {tbl} WHERE id = 1"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"tags": ["a", "b"]}

        # Delete non-existent keys should be a no-op
        result = _sql(
            conn, """SELECT '{"a":1,"b":2}'::jsonb - ARRAY['nonexistent','alsomissing']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1, "b": 2}

        # Delete keys from non-object (array) should be a no-op
        result = _sql(
            conn, """SELECT '[1,2,3]'::jsonb - ARRAY['a','b']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, 2, 3]

        # Array LHS with string elements — matching strings removed, non-strings kept
        result = _sql(
            conn, """SELECT '["a","b","c"]'::jsonb - ARRAY['a','x']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == ["b", "c"]

        result = _sql(
            conn, """SELECT '[1,"a","b"]'::jsonb - ARRAY['a']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, "b"]

        result = _sql(
            conn, """SELECT '[1,2,3]'::jsonb - ARRAY['a']"""
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == [1, 2, 3]  # no string elements, no-op


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

        # Test intermediate path behavior fixed in #1136

        # When intermediate path steps are missing, should return original value unchanged
        # even with create_missing=true (which only applies to final step)
        result = _sql(
            conn,
            """SELECT jsonb_set('{"a":1}'::jsonb, '{missing,key}', '"new"', true)""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1}  # Original value unchanged

        # With create_missing=false and intermediate path missing, also unchanged
        result = _sql(
            conn,
            """SELECT jsonb_set('{"a":1}'::jsonb, '{missing,key}', '"new"', false)""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"a": 1}

        # But when all intermediate steps exist, create_missing=true works for final step
        result = _sql(
            conn,
            """SELECT jsonb_set('{"nested":{}}'::jsonb, '{nested,newkey}', '"value"', true)""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"nested": {"newkey": "value"}}

        # And create_missing=false doesn't create final step when it's missing
        result = _sql(
            conn,
            """SELECT jsonb_set('{"nested":{}}'::jsonb, '{nested,newkey}', '"value"', false)""",
        ).scalar_one()
        if isinstance(result, str):
            result = json.loads(result)
        assert result == {"nested": {}}

    # Empty path leaves object/array documents unchanged, matching PostgreSQL.
    with db.engine.connect() as conn2:
        for target, expected in (
            ("'{\"a\":1}'", {"a": 1}),
            ("'[1,2]'", [1, 2]),
        ):
            result = _sql(
                conn2,
                f"""SELECT jsonb_set({target}::jsonb, '{{}}', '42')""",
            ).scalar_one()
            if isinstance(result, str):
                result = json.loads(result)
            assert result == expected

    # Empty path on a scalar target is an error in PostgreSQL.
    with db.engine.connect() as conn3:
        with pytest.raises(Exception) as exc_info:
            _sql(conn3, """SELECT jsonb_set('"hello"'::jsonb, '{}', '42')""")
        assert "cannot set path in scalar" in str(exc_info.value)


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

    # Test error cases now fixed in #1137 — each in a fresh connection to
    # avoid transaction state bleeding between error cases.

    # Odd number of arguments should error
    with db.engine.connect() as error_conn:
        with pytest.raises(Exception) as exc_info:
            _sql(error_conn, "SELECT jsonb_build_object('key1', 1, 'key2')")
        assert "even number of elements" in str(exc_info.value)

    # NULL key should error
    with db.engine.connect() as error_conn:
        with pytest.raises(Exception) as exc_info:
            _sql(error_conn, "SELECT jsonb_build_object(NULL, 'value')")
        assert "key must not be null" in str(exc_info.value)

from __future__ import annotations

import pytest
from sqlalchemy import literal, select, text
from sqlalchemy.orm import Session


# ---------------------------------------------------------------------------
# Group 1: SQLAlchemy expression-level .like() / .ilike() with escape=
# ---------------------------------------------------------------------------
# These exercise the ORM operator path that Dify uses in keyword search.


class TestSQLAlchemyLikeExpressions:
    def test_like_escape_literal_percent(self, db):
        """literal('a%b').like(r'a\\%b', escape='\\') → True"""
        stmt = select(literal("a%b").like(r"a\%b", escape="\\"))
        with Session(db.engine) as session:
            assert session.execute(stmt).scalar() is True

    def test_like_escape_literal_underscore(self, db):
        """literal('a_b').like(r'a\\_b', escape='\\') → True"""
        stmt = select(literal("a_b").like(r"a\_b", escape="\\"))
        with Session(db.engine) as session:
            assert session.execute(stmt).scalar() is True

    def test_ilike_escape_literal_percent(self, db):
        """literal('A%b').ilike(r'a\\%b', escape='\\') → True"""
        stmt = select(literal("A%b").ilike(r"a\%b", escape="\\"))
        with Session(db.engine) as session:
            assert session.execute(stmt).scalar() is True

    def test_like_percent_wildcard_match(self, db):
        """literal('axb').like('a%b') → True because % matches any sequence."""
        stmt = select(literal("axb").like("a%b"))
        with Session(db.engine) as session:
            assert session.execute(stmt).scalar() is True

    def test_like_escape_backslash_in_value(self, db):
        r"""literal('a\b').like(r'a\\b', escape='\') → True"""
        stmt = select(literal("a\\b").like("a\\\\b", escape="\\"))
        with Session(db.engine) as session:
            assert session.execute(stmt).scalar() is True


# ---------------------------------------------------------------------------
# Group 2: ESCAPE semantics regression matrix (raw SQL)
# ---------------------------------------------------------------------------
# All expectations validated against PostgreSQL 17.7 with C.UTF-8 collation.


_ESCAPE_CASES: list[tuple[str, bool]] = [
    # Custom escape char: '%' as escape
    ("SELECT '%A' LIKE '%A' ESCAPE '%'", False),
    ("SELECT '%A' LIKE '%%A' ESCAPE '%'", True),
    # Backslash escape for literal % and _
    ("SELECT 'a%b' LIKE E'a\\\\%b' ESCAPE E'\\\\'", True),
    ("SELECT 'a_b' LIKE E'a\\\\_b' ESCAPE E'\\\\'", True),
    # Backslash escape for literal backslash
    ("SELECT E'a\\\\b' LIKE E'a\\\\\\\\b' ESCAPE E'\\\\'", True),
    # Escaped char matches the escape char itself
    ("SELECT 'A' LIKE E'\\\\A' ESCAPE E'\\\\'", True),
]


class TestEscapeSemantics:
    @pytest.mark.parametrize("sql, expected", _ESCAPE_CASES, ids=[
        "percent_escape_no_match",
        "percent_escape_match",
        "backslash_escape_literal_percent",
        "backslash_escape_literal_underscore",
        "backslash_escape_literal_backslash",
        "backslash_escape_prefix",
    ])
    def test_escape_case(self, db, sql: str, expected: bool):
        with Session(db.engine) as session:
            result = session.execute(text(sql)).scalar()
            assert result is expected


# ---------------------------------------------------------------------------
# Group 3: NULL and negation behavior
# ---------------------------------------------------------------------------
# PG 17.7: NULL propagates through LIKE/ILIKE; NOT LIKE / NOT ILIKE invert.


class TestNullAndNegation:
    def test_null_like_pattern_is_null(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT (NULL::text LIKE '%') IS NULL")
            ).scalar()
            assert result is True

    def test_value_like_null_is_null(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT ('hello' LIKE NULL::text) IS NULL")
            ).scalar()
            assert result is True

    def test_not_ilike_positive(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'Hello' NOT ILIKE 'world'")
            ).scalar()
            assert result is True

    def test_not_ilike_negative(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'Hello' NOT ILIKE 'hello'")
            ).scalar()
            assert result is False

    def test_not_like_positive(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'abc' NOT LIKE 'xyz'")
            ).scalar()
            assert result is True

    def test_not_like_negative(self, db):
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'abc' NOT LIKE 'abc'")
            ).scalar()
            assert result is False


# ---------------------------------------------------------------------------
# Group 4: Unicode / collation behavior
# ---------------------------------------------------------------------------
# All expectations backed by PG 17.7 with C.UTF-8 collation.


class TestUnicodeIlike:
    def test_ilike_cafe_accent(self, db):
        """CAFÉ ILIKE café → True (C.UTF-8 case folding)"""
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'CAFÉ' ILIKE 'café'")
            ).scalar()
            assert result is True

    def test_ilike_eszett_no_expansion(self, db):
        """straße ILIKE STRASSE → False (C.UTF-8 does not expand ß to SS)"""
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'straße' ILIKE 'STRASSE'")
            ).scalar()
            assert result is False

    def test_ilike_tilde_n(self, db):
        """Ñ ILIKE ñ → True"""
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'Ñ' ILIKE 'ñ'")
            ).scalar()
            assert result is True

    @pytest.mark.xfail(reason="db9 divergence: Rust to_lowercase maps İ→i̇ not i, PG 17.7 C.UTF-8 returns True")
    def test_ilike_turkish_dotted_i(self, db):
        """İ ILIKE i → True (PG 17.7 C.UTF-8 case folding)."""
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'İ' ILIKE 'i'")
            ).scalar()
            assert result is True

    def test_ilike_turkish_dotless_i(self, db):
        """I ILIKE ı → False (PG 17.7 C.UTF-8: no Turkish locale folding)"""
        with Session(db.engine) as session:
            result = session.execute(
                text("SELECT 'I' ILIKE 'ı'")
            ).scalar()
            assert result is False

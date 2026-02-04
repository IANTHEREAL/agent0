from __future__ import annotations

from sqlalchemy import JSON, Integer, String, cast, func, or_, select
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import Mapped, Session, declarative_base, mapped_column

from dify_sqlalchemy_compat.upstream_helpers import escape_like_pattern


def test_json_keyword_search_pattern(schema, db):
    # Upstream: dify@acfd34e8767c3f7c887f99c51763f6150ff21898
    # api/controllers/console/datasets/datasets_segments.py#L155
    base = declarative_base()

    class DocumentSegment(base):
        __tablename__ = "document_segments"
        __table_args__ = {"schema": schema}

        id: Mapped[int] = mapped_column(Integer, primary_key=True)
        content: Mapped[str] = mapped_column(String, nullable=False)
        keywords: Mapped[list[str]] = mapped_column(JSON, nullable=False)

    base.metadata.create_all(db.engine)

    with Session(db.engine, expire_on_commit=False) as session:
        session.add_all(
            [
                DocumentSegment(id=1, content="unrelated", keywords=["test_data"]),
                DocumentSegment(id=2, content="unrelated", keywords=["testXdata"]),
                DocumentSegment(id=3, content="unrelated", keywords=["other"]),
            ]
        )
        session.commit()

    keyword = "test_data"
    escaped_keyword = escape_like_pattern(keyword)

    # Mirror Dify's PostgreSQL jsonb_array_elements_text + ARRAY(subquery) + ILIKE ESCAPE pattern.
    keywords_condition = func.array_to_string(
        func.array(
            select(func.jsonb_array_elements_text(cast(DocumentSegment.keywords, JSONB)))
            .correlate(DocumentSegment)
            .scalar_subquery()
        ),
        ",",
    ).ilike(f"%{escaped_keyword}%", escape="\\")

    with Session(db.engine, expire_on_commit=False) as session:
        matched_ids = session.execute(
            select(DocumentSegment.id)
            .where(
                or_(
                    DocumentSegment.content.ilike(f"%{escaped_keyword}%", escape="\\"),
                    keywords_condition,
                )
            )
            .order_by(DocumentSegment.id)
        ).scalars()
        assert list(matched_ids) == [1]

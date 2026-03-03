"""Database operations for RAG using SQLAlchemy ORM."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import select, text
from sqlalchemy.engine import Engine
from sqlalchemy.orm import Session, sessionmaker

from .models import Base, Document


@dataclass
class RetrievedDocument:
    id: int
    source: str
    content: str
    score: float
    meta: dict
    method: str
    details: dict[str, Any] = field(default_factory=dict)


def build_engine(database_url: str) -> Engine:
    from sqlalchemy import create_engine

    normalized_url = database_url.strip()
    if normalized_url.startswith("postgresql://"):
        normalized_url = "postgresql+psycopg://" + normalized_url[len("postgresql://") :]
    elif normalized_url.startswith("postgres://"):
        normalized_url = "postgresql+psycopg://" + normalized_url[len("postgres://") :]

    return create_engine(normalized_url, pool_pre_ping=True)


def init_schema(engine: Engine) -> None:
    """Create vector extension when needed and ensure table exists."""
    with engine.begin() as conn:
        try:
            conn.execute(text("CREATE EXTENSION IF NOT EXISTS vector"))
        except Exception:
            # db9 has vector built in, so extension creation can fail safely.
            pass
        Base.metadata.create_all(conn)
        # FTS expression index for keyword-heavy retrieval.
        conn.execute(
            text(
                "CREATE INDEX IF NOT EXISTS idx_rag_documents_fts "
                "ON rag_documents USING GIN (to_tsvector('english', content))"
            )
        )
        # HNSW index for fast approximate vector search (ANN with cosine distance).
        conn.execute(
            text(
                "CREATE INDEX IF NOT EXISTS idx_rag_documents_embedding "
                "ON rag_documents USING hnsw (embedding vector_cosine_ops) "
                "WITH (m = 16, ef_construction = 64)"
            )
        )


def build_session_factory(engine: Engine) -> sessionmaker[Session]:
    return sessionmaker(bind=engine, expire_on_commit=False)


def ingest_chunks(
    session: Session,
    *,
    source: str,
    chunks: list[str],
    embeddings: list[list[float]],
) -> tuple[int, int]:
    """Insert new chunks and skip duplicates by content hash."""
    if len(chunks) != len(embeddings):
        raise ValueError("chunks and embeddings length mismatch")

    inserted = 0
    skipped = 0
    for content, embedding in zip(chunks, embeddings):
        normalized = content.strip()
        if not normalized:
            continue
        digest = hashlib.sha256(normalized.encode("utf-8")).hexdigest()
        existing = session.scalar(select(Document.id).where(Document.content_hash == digest))
        if existing is not None:
            skipped += 1
            continue
        session.add(
            Document(
                source=source,
                content=normalized,
                content_hash=digest,
                embedding=embedding,
                meta={"chars": len(normalized)},
            )
        )
        inserted += 1

    session.commit()
    return inserted, skipped


def search_vector_documents(
    session: Session,
    *,
    query_embedding: list[float],
    top_k: int,
    source: str | None = None,
) -> list[RetrievedDocument]:
    distance = Document.embedding.cosine_distance(query_embedding)
    stmt = select(Document, distance.label("distance"))
    if source is not None:
        stmt = stmt.where(Document.source == source)
    stmt = stmt.order_by(distance).limit(top_k)
    rows = session.execute(stmt).all()
    return [
        RetrievedDocument(
            id=doc.id,
            source=doc.source,
            content=doc.content,
            score=1.0 - float(distance),
            meta=doc.meta,
            method="vector",
            details={"cosine_distance": float(distance)},
        )
        for doc, distance in rows
    ]


def search_fts_documents(
    session: Session,
    *,
    query_text: str,
    top_k: int,
    source: str | None = None,
) -> list[RetrievedDocument]:
    sql = (
        "SELECT id, source, content, metadata, "
        "ts_rank(to_tsvector('english', content), websearch_to_tsquery('english', :query_text)) AS rank "
        "FROM rag_documents "
        "WHERE to_tsvector('english', content) @@ websearch_to_tsquery('english', :query_text)"
    )
    params: dict[str, Any] = {"query_text": query_text, "top_k": top_k}
    if source is not None:
        sql += " AND source = :source"
        params["source"] = source
    sql += " ORDER BY rank DESC LIMIT :top_k"

    rows = session.execute(text(sql), params).mappings().all()
    docs: list[RetrievedDocument] = []
    for row in rows:
        rank = float(row["rank"])
        docs.append(
            RetrievedDocument(
                id=int(row["id"]),
                source=str(row["source"]),
                content=str(row["content"]),
                score=rank,
                meta=dict(row["metadata"]) if isinstance(row["metadata"], dict) else {},
                method="fts",
                details={"ts_rank": rank},
            )
        )
    return docs


def search_dual_fuse_documents(
    session: Session,
    *,
    query_text: str,
    query_embedding: list[float],
    top_k: int,
    source: str | None = None,
    vector_pool_k: int | None = None,
    fts_pool_k: int | None = None,
    rrf_k: int = 60,
) -> list[RetrievedDocument]:
    vector_hits = search_vector_documents(
        session,
        query_embedding=query_embedding,
        top_k=vector_pool_k or max(top_k * 3, top_k),
        source=source,
    )
    fts_hits = search_fts_documents(
        session,
        query_text=query_text,
        top_k=fts_pool_k or max(top_k * 3, top_k),
        source=source,
    )

    fused: dict[int, dict[str, Any]] = {}

    for rank, hit in enumerate(vector_hits, start=1):
        entry = fused.setdefault(
            hit.id,
            {
                "doc": hit,
                "rrf": 0.0,
                "vector_rank": None,
                "vector_score": None,
                "fts_rank": None,
                "fts_score": None,
            },
        )
        entry["rrf"] += 1.0 / (rrf_k + rank)
        entry["vector_rank"] = rank
        entry["vector_score"] = hit.score

    for rank, hit in enumerate(fts_hits, start=1):
        entry = fused.setdefault(
            hit.id,
            {
                "doc": hit,
                "rrf": 0.0,
                "vector_rank": None,
                "vector_score": None,
                "fts_rank": None,
                "fts_score": None,
            },
        )
        entry["rrf"] += 1.0 / (rrf_k + rank)
        entry["fts_rank"] = rank
        entry["fts_score"] = hit.score

    ranked = sorted(fused.values(), key=lambda item: item["rrf"], reverse=True)[:top_k]
    results: list[RetrievedDocument] = []
    for item in ranked:
        doc = item["doc"]
        results.append(
            RetrievedDocument(
                id=doc.id,
                source=doc.source,
                content=doc.content,
                score=float(item["rrf"]),
                meta=doc.meta,
                method="dual_fuse",
                details={
                    "rrf": float(item["rrf"]),
                    "vector_rank": item["vector_rank"],
                    "vector_score": item["vector_score"],
                    "fts_rank": item["fts_rank"],
                    "fts_score": item["fts_score"],
                },
            )
        )
    return results


def search_documents(
    session: Session, *, query_embedding: list[float], top_k: int
) -> list[RetrievedDocument]:
    """Backward-compatible alias (vector-only)."""
    return search_vector_documents(session, query_embedding=query_embedding, top_k=top_k)

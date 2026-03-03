"""Load and scenario benchmarking for vector / FTS / dual-fuse RAG."""

from __future__ import annotations

import concurrent.futures
import random
import statistics
import time
from dataclasses import dataclass
from typing import Any, Literal

from sqlalchemy import func, select
from sqlalchemy.orm import Session, sessionmaker

from .db import (
    RetrievedDocument,
    ingest_chunks,
    search_dual_fuse_documents,
    search_fts_documents,
    search_vector_documents,
)
from .llm import OpenAIClient
from .models import Document

RetrievalMode = Literal["vector", "fts", "dual_fuse"]


@dataclass
class SyntheticDoc:
    source: str
    content: str


@dataclass
class ScenarioSpec:
    name: str
    mode: RetrievalMode
    query: str
    expected_terms: list[str]
    source_filter: str | None = None
    generate_answer: bool = True


def _percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = int((len(ordered) - 1) * pct)
    return ordered[idx]


def _snip(text: str, limit: int = 140) -> str:
    text = " ".join(text.split())
    return text if len(text) <= limit else text[: limit - 3] + "..."


def generate_synthetic_docs(doc_count: int, *, seed: int = 42) -> list[SyntheticDoc]:
    rng = random.Random(seed)
    topic_templates = [
        (
            "db",
            "db9 semantic retrieval uses vector embeddings, cosine distance, and ranked candidate sets.",
        ),
        (
            "ops",
            "Incident response runbook recommends rollback, canary analysis, and postmortem checkpoints.",
        ),
        (
            "security",
            "API token rotation and least-privilege access are mandatory for production workloads.",
        ),
        (
            "billing",
            "Billing pipeline computes invoice totals, quota overages, and monthly plan reconciliation.",
        ),
        (
            "rag",
            "RAG retrieves relevant chunks first, then generates grounded answers with citations.",
        ),
        (
            "network",
            "Network reliability depends on retry policies, latency budgets, and circuit breaker thresholds.",
        ),
    ]

    docs: list[SyntheticDoc] = []
    for i in range(doc_count):
        source, template = topic_templates[i % len(topic_templates)]
        doc_key = f"doc{i:03d}"
        detail = (
            f"DOC-{i} "
            f"doc_key={doc_key} "
            f"region=us-west-{1 + (i % 2)} "
            f"service={'api' if i % 3 == 0 else 'worker'} "
            f"priority={1 + (i % 4)} "
            f"note={rng.choice(['stable', 'degraded', 'recovering'])}."
        )
        docs.append(SyntheticDoc(source=source, content=f"{template} {detail}"))
    return docs


def _vector_like_load_queries() -> list[str]:
    return [
        "How does db9 semantic retrieval rank vector matches?",
        "incident rollback runbook for production outage",
        "least privilege token rotation policy",
        "invoice quota overage monthly plan details",
        "RAG grounded answer with retrieved chunks",
        "latency budget and retry policy tuning",
        "DOC-42 rollback and postmortem checkpoints",
        "cosine distance vector ranking in postgres",
    ]


def _fts_load_queries() -> list[str]:
    return [
        "incident rollback canary postmortem",
        "invoice totals quota overages reconciliation",
        "least privilege token rotation",
        "vector embeddings cosine distance",
        "doc043 incident runbook",
        "retry policy latency budget circuit breaker",
    ]


def _scenario_specs() -> list[ScenarioSpec]:
    return [
        ScenarioSpec(
            name="semantic_rag_pipeline",
            mode="vector",
            query="How does retrieval augmented generation produce grounded answers?",
            expected_terms=["RAG", "grounded answers"],
        ),
        ScenarioSpec(
            name="fts_exact_doc_lookup",
            mode="fts",
            query="doc043 incident rollback runbook",
            expected_terms=["doc_key=doc043", "rollback"],
            generate_answer=False,
        ),
        ScenarioSpec(
            name="dual_db_vector_keyword_mix",
            mode="dual_fuse",
            query="db9 vector embeddings cosine ranked candidate sets",
            expected_terms=["vector embeddings", "cosine distance"],
        ),
        ScenarioSpec(
            name="dual_security_policy",
            mode="dual_fuse",
            query="How should API token rotation and least privilege be enforced?",
            expected_terms=["token rotation", "least-privilege"],
        ),
        ScenarioSpec(
            name="source_filter_ops",
            mode="vector",
            query="How to handle rollback in incident response?",
            expected_terms=["rollback"],
            source_filter="ops",
            generate_answer=False,
        ),
        ScenarioSpec(
            name="source_filter_billing_fts",
            mode="fts",
            query="invoice quota overages reconciliation",
            expected_terms=["invoice", "overages"],
            source_filter="billing",
            generate_answer=False,
        ),
        ScenarioSpec(
            name="out_of_scope_probe",
            mode="dual_fuse",
            query="quantum entanglement calibration in superconducting qubits",
            expected_terms=[],
        ),
    ]


def _retrieve(
    session: Session,
    *,
    mode: RetrievalMode,
    query_text: str,
    query_embedding: list[float] | None,
    top_k: int,
    source_filter: str | None = None,
) -> list[RetrievedDocument]:
    if mode == "vector":
        if query_embedding is None:
            raise ValueError("query_embedding is required for vector mode")
        return search_vector_documents(
            session,
            query_embedding=query_embedding,
            top_k=top_k,
            source=source_filter,
        )
    if mode == "fts":
        return search_fts_documents(
            session,
            query_text=query_text,
            top_k=top_k,
            source=source_filter,
        )
    if mode == "dual_fuse":
        if query_embedding is None:
            raise ValueError("query_embedding is required for dual_fuse mode")
        return search_dual_fuse_documents(
            session,
            query_text=query_text,
            query_embedding=query_embedding,
            top_k=top_k,
            source=source_filter,
        )
    raise ValueError(f"unsupported mode: {mode}")


def _contains_expected(documents: list[RetrievedDocument], expected_terms: list[str]) -> bool:
    if not expected_terms:
        return True
    lowered = [term.lower() for term in expected_terms]
    for doc in documents:
        text = doc.content.lower()
        if all(term in text for term in lowered):
            return True
    return False


def _run_load_test(
    *,
    session_factory: sessionmaker[Session],
    mode: RetrievalMode,
    queries: list[str],
    query_embeddings: dict[str, list[float]],
    top_k: int,
    concurrency: int,
    requests: int,
) -> dict[str, Any]:
    latencies_ms: list[float] = []
    completed = 0
    exceptions = 0
    non_empty_hits = 0

    def worker(i: int) -> tuple[bool, bool, float]:
        query_text = queries[i % len(queries)]
        query_embedding = query_embeddings.get(query_text)
        start = time.perf_counter()
        try:
            with session_factory() as session:
                docs = _retrieve(
                    session,
                    mode=mode,
                    query_text=query_text,
                    query_embedding=query_embedding,
                    top_k=top_k,
                )
            has_hits = len(docs) > 0
            return True, has_hits, (time.perf_counter() - start) * 1000.0
        except Exception:
            return False, False, (time.perf_counter() - start) * 1000.0

    begin = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = [pool.submit(worker, i) for i in range(requests)]
        for fut in concurrent.futures.as_completed(futures):
            ok, has_hits, latency_ms = fut.result()
            latencies_ms.append(latency_ms)
            if ok:
                completed += 1
                if has_hits:
                    non_empty_hits += 1
            else:
                exceptions += 1
    elapsed = time.perf_counter() - begin

    return {
        "mode": mode,
        "requests": requests,
        "concurrency": concurrency,
        "completed": completed,
        "exceptions": exceptions,
        "completion_rate": (completed / requests) if requests else 0.0,
        "non_empty_hits": non_empty_hits,
        "non_empty_hit_rate": (non_empty_hits / requests) if requests else 0.0,
        "qps": (requests / elapsed) if elapsed > 0 else 0.0,
        "latency_ms_p50": _percentile(latencies_ms, 0.50),
        "latency_ms_p95": _percentile(latencies_ms, 0.95),
        "latency_ms_p99": _percentile(latencies_ms, 0.99),
        "latency_ms_avg": statistics.mean(latencies_ms) if latencies_ms else 0.0,
    }


def run_benchmark(
    *,
    session_factory: sessionmaker[Session],
    llm: OpenAIClient,
    docs: int,
    top_k: int,
    concurrency: int,
    requests_per_mode: int,
    embed_batch_size: int,
) -> dict[str, Any]:
    synthetic_docs = generate_synthetic_docs(docs)
    corpus_texts = [doc.content for doc in synthetic_docs]
    corpus_embeddings = llm.embed_texts_batched(corpus_texts, batch_size=embed_batch_size)

    grouped: dict[str, list[tuple[str, list[float]]]] = {}
    for doc, emb in zip(synthetic_docs, corpus_embeddings):
        grouped.setdefault(doc.source, []).append((doc.content, emb))

    inserted_total = 0
    skipped_total = 0
    with session_factory() as session:
        for source, pairs in grouped.items():
            chunks = [c for c, _ in pairs]
            embeddings = [e for _, e in pairs]
            inserted, skipped = ingest_chunks(
                session,
                source=source,
                chunks=chunks,
                embeddings=embeddings,
            )
            inserted_total += inserted
            skipped_total += skipped

    vector_queries = _vector_like_load_queries()
    fts_queries = _fts_load_queries()
    scenario_specs = _scenario_specs()

    emb_queries = sorted(
        {
            *vector_queries,
            *fts_queries,
            *[spec.query for spec in scenario_specs if spec.mode in ("vector", "dual_fuse")],
        }
    )
    emb_vectors = llm.embed_texts_batched(emb_queries, batch_size=min(embed_batch_size, 64))
    query_embeddings = {query: emb for query, emb in zip(emb_queries, emb_vectors)}

    load_results = []
    for mode in ("vector", "fts", "dual_fuse"):
        mode_queries = fts_queries if mode == "fts" else vector_queries
        load_results.append(
            _run_load_test(
                session_factory=session_factory,
                mode=mode,
                queries=mode_queries,
                query_embeddings=query_embeddings,
                top_k=top_k,
                concurrency=concurrency,
                requests=requests_per_mode,
            )
        )

    scenario_results = []
    for spec in scenario_specs:
        with session_factory() as session:
            docs_out = _retrieve(
                session,
                mode=spec.mode,
                query_text=spec.query,
                query_embedding=query_embeddings.get(spec.query),
                top_k=top_k,
                source_filter=spec.source_filter,
            )

        answer_preview = None
        if spec.generate_answer and docs_out:
            answer = llm.answer(question=spec.query, context_chunks=[doc.content for doc in docs_out])
            answer_preview = _snip(answer, limit=220)

        scenario_results.append(
            {
                "name": spec.name,
                "mode": spec.mode,
                "query": spec.query,
                "source_filter": spec.source_filter,
                "hit_count": len(docs_out),
                "matched_expected": _contains_expected(docs_out, spec.expected_terms),
                "top_results": [
                    {
                        "id": doc.id,
                        "source": doc.source,
                        "score": doc.score,
                        "method": doc.method,
                        "snippet": _snip(doc.content, 120),
                    }
                    for doc in docs_out[:3]
                ],
                "answer_preview": answer_preview,
            }
        )

    with session_factory() as session:
        total_docs = session.scalar(select(func.count()).select_from(Document)) or 0

    return {
        "ingest": {
            "inserted": inserted_total,
            "skipped": skipped_total,
            "total_docs_after": int(total_docs),
            "sources": sorted(grouped.keys()),
        },
        "load_tests": load_results,
        "scenarios": scenario_results,
    }

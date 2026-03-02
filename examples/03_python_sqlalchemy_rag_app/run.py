"""CLI entrypoint for db9 RAG app with SQLAlchemy ORM."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Literal

from dotenv import load_dotenv

from rag_sqlalchemy.benchmark import run_benchmark
from rag_sqlalchemy.db import (
    build_engine,
    build_session_factory,
    ingest_chunks,
    init_schema,
    search_dual_fuse_documents,
    search_fts_documents,
    search_vector_documents,
)
from rag_sqlalchemy.llm import OpenAIClient

RetrievalMode = Literal["vector", "fts", "dual_fuse"]

DEFAULT_KNOWLEDGE = [
    "db9 is serverless Postgres for agents and supports PostgreSQL-compatible SQL.",
    "db9 includes pgvector-compatible vector search with cosine (<=>), L2 (<->), and inner product (<#>) distances.",
    "db9 also supports PostgreSQL full-text search, so keyword-heavy retrieval can be done with to_tsvector and tsquery.",
    "You can connect to staging db9 with a PostgreSQL DSN from db9 db connect <id> using host pg.staging.db9.ai port 5433.",
    "RAG combines embedding-based retrieval with a generation model that answers using the retrieved context.",
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="RAG app on staging.db9.ai with SQLAlchemy ORM")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("init", help="Create required schema")

    ingest = sub.add_parser("ingest", help="Ingest documents from a text file")
    ingest.add_argument("--file", required=True, help="Input text file path; chunks are split by blank lines")
    ingest.add_argument("--source", default="file", help="Document source tag")

    ask = sub.add_parser("ask", help="Ask a question via RAG")
    ask.add_argument("question", help="User question")
    ask.add_argument(
        "--mode",
        choices=["vector", "fts", "dual_fuse"],
        default="dual_fuse",
        help="Retrieval mode: vector, fts, or dual_fuse (vector + FTS RRF fusion)",
    )
    ask.add_argument("--top-k", type=int, default=4, help="Number of retrieved chunks")
    ask.add_argument("--source", default=None, help="Optional source filter (exact match)")

    demo = sub.add_parser("demo", help="Run a full demo using built-in knowledge")
    demo.add_argument("question", nargs="?", default="How can I do semantic search on db9?")
    demo.add_argument(
        "--mode",
        choices=["vector", "fts", "dual_fuse"],
        default="dual_fuse",
        help="Retrieval mode for demo query",
    )
    demo.add_argument("--top-k", type=int, default=3, help="Number of retrieved chunks")
    demo.add_argument("--source", default=None, help="Optional source filter (exact match)")

    bench = sub.add_parser("benchmark", help="Run volume + concurrency + multi-scenario benchmark")
    bench.add_argument("--docs", type=int, default=240, help="Number of synthetic docs to ingest")
    bench.add_argument("--top-k", type=int, default=5, help="Top-k for retrieval")
    bench.add_argument("--concurrency", type=int, default=12, help="Concurrent workers per mode")
    bench.add_argument("--requests-per-mode", type=int, default=60, help="Requests for each mode load test")
    bench.add_argument("--embed-batch-size", type=int, default=96, help="Embedding batch size")
    bench.add_argument("--report-file", default=None, help="Optional JSON report output path")
    return parser.parse_args()


def split_chunks(text: str) -> list[str]:
    chunks = [chunk.strip() for chunk in text.split("\n\n")]
    return [chunk for chunk in chunks if chunk]


def must_get_env(name: str) -> str:
    value = os.getenv(name)
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def cmd_init(database_url: str) -> None:
    engine = build_engine(database_url)
    init_schema(engine)
    print("Schema ready: rag_documents")


def cmd_ingest(database_url: str, api_key: str, embed_model: str, file_path: str, source: str) -> None:
    engine = build_engine(database_url)
    init_schema(engine)
    session_factory = build_session_factory(engine)
    llm = OpenAIClient(api_key=api_key, embed_model=embed_model, chat_model="gpt-4o-mini")

    text = Path(file_path).read_text(encoding="utf-8")
    chunks = split_chunks(text)
    if not chunks:
        print("No chunks found in file.")
        return

    embeddings = llm.embed_texts_batched(chunks, batch_size=96)
    with session_factory() as session:
        inserted, skipped = ingest_chunks(session, source=source, chunks=chunks, embeddings=embeddings)
    print(f"Ingest done. inserted={inserted} skipped={skipped}")


def _retrieve_docs(
    *,
    session,
    mode: RetrievalMode,
    question: str,
    question_embedding: list[float] | None,
    top_k: int,
    source: str | None,
):
    if mode == "vector":
        if question_embedding is None:
            raise ValueError("question_embedding is required in vector mode")
        return search_vector_documents(
            session,
            query_embedding=question_embedding,
            top_k=top_k,
            source=source,
        )
    if mode == "fts":
        return search_fts_documents(
            session,
            query_text=question,
            top_k=top_k,
            source=source,
        )
    if mode == "dual_fuse":
        if question_embedding is None:
            raise ValueError("question_embedding is required in dual_fuse mode")
        return search_dual_fuse_documents(
            session,
            query_text=question,
            query_embedding=question_embedding,
            top_k=top_k,
            source=source,
        )
    raise ValueError(f"Unsupported mode: {mode}")


def cmd_ask(
    database_url: str,
    api_key: str,
    embed_model: str,
    chat_model: str,
    question: str,
    mode: RetrievalMode,
    top_k: int,
    source: str | None,
) -> None:
    engine = build_engine(database_url)
    session_factory = build_session_factory(engine)
    llm = OpenAIClient(api_key=api_key, embed_model=embed_model, chat_model=chat_model)

    question_embedding = llm.embed_text(question) if mode in ("vector", "dual_fuse") else None
    with session_factory() as session:
        docs = _retrieve_docs(
            session=session,
            mode=mode,
            question=question,
            question_embedding=question_embedding,
            top_k=top_k,
            source=source,
        )

    if not docs:
        print("No documents found. Ingest documents first.")
        return

    print(f"Mode: {mode}")
    print("Retrieved context:")
    for idx, doc in enumerate(docs, start=1):
        snippet = doc.content[:140].replace("\n", " ")
        print(f"[{idx}] score={doc.score:.4f} source={doc.source} method={doc.method} text={snippet}")

    answer = llm.answer(question=question, context_chunks=[d.content for d in docs])
    print("\nAnswer:\n")
    print(answer)


def cmd_demo(
    database_url: str,
    api_key: str,
    embed_model: str,
    chat_model: str,
    question: str,
    mode: RetrievalMode,
    top_k: int,
    source: str | None,
) -> None:
    engine = build_engine(database_url)
    init_schema(engine)
    session_factory = build_session_factory(engine)
    llm = OpenAIClient(api_key=api_key, embed_model=embed_model, chat_model=chat_model)
    embeddings = llm.embed_texts_batched(DEFAULT_KNOWLEDGE, batch_size=32)
    with session_factory() as session:
        ingest_chunks(session, source="demo", chunks=DEFAULT_KNOWLEDGE, embeddings=embeddings)
    cmd_ask(
        database_url=database_url,
        api_key=api_key,
        embed_model=embed_model,
        chat_model=chat_model,
        question=question,
        mode=mode,
        top_k=top_k,
        source=source,
    )


def cmd_benchmark(
    database_url: str,
    api_key: str,
    embed_model: str,
    chat_model: str,
    *,
    docs: int,
    top_k: int,
    concurrency: int,
    requests_per_mode: int,
    embed_batch_size: int,
    report_file: str | None,
) -> None:
    engine = build_engine(database_url)
    init_schema(engine)
    session_factory = build_session_factory(engine)
    llm = OpenAIClient(api_key=api_key, embed_model=embed_model, chat_model=chat_model)

    report = run_benchmark(
        session_factory=session_factory,
        llm=llm,
        docs=docs,
        top_k=top_k,
        concurrency=concurrency,
        requests_per_mode=requests_per_mode,
        embed_batch_size=embed_batch_size,
    )

    print("Benchmark summary")
    print("=" * 72)
    ingest = report["ingest"]
    print(
        f"Ingest: inserted={ingest['inserted']} skipped={ingest['skipped']} "
        f"total_docs_after={ingest['total_docs_after']}"
    )
    print()

    print("Load tests:")
    for item in report["load_tests"]:
        print(
            f"- mode={item['mode']} req={item['requests']} conc={item['concurrency']} "
            f"completed={item['completed']} exceptions={item['exceptions']} "
            f"non_empty={item['non_empty_hits']} "
            f"qps={item['qps']:.2f} p50={item['latency_ms_p50']:.2f}ms "
            f"p95={item['latency_ms_p95']:.2f}ms p99={item['latency_ms_p99']:.2f}ms"
        )
    print()

    print("Scenario checks:")
    for scenario in report["scenarios"]:
        print(
            f"- {scenario['name']} mode={scenario['mode']} hits={scenario['hit_count']} "
            f"matched_expected={scenario['matched_expected']}"
        )
        if scenario["top_results"]:
            top = scenario["top_results"][0]
            print(
                f"  top1 source={top['source']} score={top['score']:.4f} "
                f"snippet={top['snippet']}"
            )
        if scenario["answer_preview"]:
            print(f"  answer={scenario['answer_preview']}")

    if report_file:
        Path(report_file).write_text(json.dumps(report, indent=2), encoding="utf-8")
        print()
        print(f"Report written: {report_file}")


def main() -> int:
    load_dotenv()
    args = parse_args()

    try:
        database_url = must_get_env("DATABASE_URL")
        embed_model = os.getenv("OPENAI_EMBED_MODEL", "text-embedding-3-small")
        chat_model = os.getenv("OPENAI_CHAT_MODEL", "gpt-4o-mini")

        if args.command == "init":
            cmd_init(database_url)
            return 0

        api_key = must_get_env("OPENAI_API_KEY")
        if args.command == "ingest":
            cmd_ingest(database_url, api_key, embed_model, args.file, args.source)
            return 0
        if args.command == "ask":
            cmd_ask(
                database_url,
                api_key,
                embed_model,
                chat_model,
                args.question,
                args.mode,
                args.top_k,
                args.source,
            )
            return 0
        if args.command == "demo":
            cmd_demo(
                database_url,
                api_key,
                embed_model,
                chat_model,
                args.question,
                args.mode,
                args.top_k,
                args.source,
            )
            return 0
        if args.command == "benchmark":
            cmd_benchmark(
                database_url=database_url,
                api_key=api_key,
                embed_model=embed_model,
                chat_model=chat_model,
                docs=args.docs,
                top_k=args.top_k,
                concurrency=args.concurrency,
                requests_per_mode=args.requests_per_mode,
                embed_batch_size=args.embed_batch_size,
                report_file=args.report_file,
            )
            return 0
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    return 1


if __name__ == "__main__":
    raise SystemExit(main())

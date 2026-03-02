# RAG App on `staging.db9.ai` with SQLAlchemy ORM

This example builds a minimal Retrieval-Augmented Generation (RAG) app on db9 (`https://staging.db9.ai`) using:

- **PostgreSQL ORM**: **SQLAlchemy 2.x** (chosen for maturity, ecosystem, and clean pgvector integration)
- **Vector type**: `pgvector` SQLAlchemy type
- **FTS**: PostgreSQL full-text search (`to_tsvector`, `websearch_to_tsquery`, `ts_rank`)
- **Dual-fuse retrieval**: vector + FTS fusion with Reciprocal Rank Fusion (RRF)
- **LLM**: OpenAI embeddings + chat completion

## Why SQLAlchemy here

For a PostgreSQL-backed RAG app, SQLAlchemy is a strong default:

- battle-tested ORM and query layer
- strong PostgreSQL support
- easy extension with custom operators/functions
- direct compatibility with `pgvector` via `pgvector.sqlalchemy.Vector`

## Prerequisites

- Python 3.10+
- `uv` (recommended) or `pip`
- db9 CLI authenticated against staging:

```bash
curl -fsSL https://staging.db9.ai/install | sh
db9 login
db9 db create --name rag-app
db9 db connect <your-db-id>
```

Copy the returned DSN to `DATABASE_URL`.

## Setup

```bash
cd examples/03_python_sqlalchemy_rag_app
cp .env.example .env
# edit .env: DATABASE_URL + OPENAI_API_KEY
uv sync
```

## Commands

Initialize schema:

```bash
uv run python run.py init
```

Ingest your own text file (split by blank lines into chunks):

```bash
uv run python run.py ingest --file ./knowledge.txt --source handbook
```

Ask a question:

```bash
uv run python run.py ask "How do I run semantic search on db9?" --mode dual_fuse --top-k 4
```

Run end-to-end demo (auto-ingests built-in knowledge first):

```bash
uv run python run.py demo "How does db9 support vector search?" --mode dual_fuse
```

Run volume + concurrency + scenario benchmark:

```bash
uv run python run.py benchmark \
  --docs 300 \
  --concurrency 16 \
  --requests-per-mode 90 \
  --top-k 5 \
  --report-file /tmp/rag_benchmark_report.json
```

`benchmark` covers:

- higher-volume ingestion with batched embeddings
- concurrent retrieval load test for `vector`, `fts`, and `dual_fuse`
- multi-scenario RAG checks (semantic, keyword exact-match, source-filtered, out-of-scope probe)

## Environment Variables

- `DATABASE_URL` (required): db9 Postgres DSN, usually host `pg.staging.db9.ai`, port `5433`
- `OPENAI_API_KEY` (required for `ingest/ask/demo`)
- `OPENAI_EMBED_MODEL` (optional, default `text-embedding-3-small`)
- `OPENAI_CHAT_MODEL` (optional, default `gpt-4o-mini`)

## Notes

- On db9, vector support is built in. `CREATE EXTENSION vector` may be skipped safely.
- The app de-duplicates chunks by SHA-256 content hash.
- Retrieval modes:
  - `vector`: cosine similarity (`score = 1 - distance`)
  - `fts`: `ts_rank` keyword relevance
  - `dual_fuse`: RRF fusion of vector + FTS rankings

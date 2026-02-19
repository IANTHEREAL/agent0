# RAG Ingest Pipeline Example

Reads text files from fs9, chunks them, generates embeddings via any OpenAI-compatible API, and stores the results in pg-tikv with vector similarity search and GIN full-text search support.

## Prerequisites

- Node.js 18+
- A running pg-tikv / db9 instance
- An embedding API key (OpenAI, or any compatible endpoint)
- Text files uploaded to fs9

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DB9_DATABASE_ID` | **Yes** | — | Target database ID |
| `EMBEDDING_API_KEY` | **Yes** | — | API key for embedding service |
| `DB9_API_URL` | No | `https://db9.shared.aws.tidbcloud.com/api` | API endpoint |
| `DB9_TOKEN` | No | Auto-registers | Auth token |
| `EMBEDDING_API_URL` | No | `https://api.openai.com/v1/embeddings` | Embedding endpoint |
| `EMBEDDING_MODEL` | No | `text-embedding-3-small` | Model name |
| `EMBEDDING_DIMENSIONS` | No | `1536` | Vector dimensions |
| `CHUNK_SIZE` | No | `1000` | Characters per chunk |
| `CHUNK_OVERLAP` | No | `200` | Overlap between chunks |
| `SOURCE_PATH` | No | `/uploads/` | fs9 directory to ingest |

## Quick Start

```bash
# Install dependencies (from sdk/ts/)
npm install

# Upload some text files to fs9 first
db9 fs upload <database-id> ./document.txt /uploads/document.txt

# Run the ingest pipeline
DB9_DATABASE_ID=<your-db-id> \
EMBEDDING_API_KEY=<your-key> \
npx tsx examples/rag_ingest.ts
```

## How It Works

1. **Connect** — fetches the database connection string via the db9 SDK, connects with `pg` driver
2. **Schema** — creates `rag_chunks` table with VECTOR and TSVECTOR columns (no extensions needed — VECTOR is built-in)
3. **Read** — lists files from fs9 at `SOURCE_PATH`, reads each file's text content
4. **Chunk** — splits text into overlapping chunks (configurable size/overlap)
5. **Embed** — sends chunks to the embedding API in batches of 128, with exponential backoff on rate limits
6. **Store** — batch-inserts chunks with embeddings and tsvectors (100 rows per INSERT, upsert on conflict)
7. **Index** — creates a GIN index on the `search_vector` column for fast full-text search

## Schema

```sql
CREATE TABLE rag_chunks (
  id SERIAL PRIMARY KEY,
  doc_path TEXT NOT NULL,
  chunk_index INTEGER NOT NULL,
  chunk_text TEXT NOT NULL,
  embedding VECTOR(1536),
  search_vector TSVECTOR,
  created_at TIMESTAMP DEFAULT NOW(),
  UNIQUE(doc_path, chunk_index)
);
```

## Hybrid Search Query

Combine vector similarity with full-text filtering:

```sql
SELECT doc_path, chunk_text,
  embedding <=> $1::vector AS distance
FROM rag_chunks
WHERE search_vector @@ plainto_tsquery('english', $2)
ORDER BY distance
LIMIT 5;
```

- `$1` — query embedding as a vector literal (e.g., `'[0.1, 0.2, ...]'`)
- `$2` — search text (e.g., `'database performance'`)
- `<=>` — cosine distance operator

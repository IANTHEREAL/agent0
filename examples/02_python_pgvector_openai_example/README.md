# Semantic Search with pgvector + OpenAI Embeddings

A tutorial demonstrating how to build a semantic search system using PostgreSQL's [pgvector](https://github.com/pgvector/pgvector) extension and OpenAI's embedding API.

## What You'll Learn

| Concept | Description |
|---------|-------------|
| **Vector Embeddings** | Convert text into numerical vectors that capture semantic meaning |
| **pgvector Extension** | Store and query vectors directly in PostgreSQL |
| **Cosine Similarity** | Find semantically similar documents using `<=>` operator |
| **L2 Distance** | Alternative distance metric using `<->` operator |
| **IVFFlat Index** | Approximate nearest neighbor index for fast similarity search |
| **Batch Embeddings** | Generate multiple embeddings in a single OpenAI API call |

## Prerequisites

- Python 3.10+
- PostgreSQL with [pgvector extension](https://github.com/pgvector/pgvector) installed
- OpenAI API key ([get one here](https://platform.openai.com/api-keys))
- [uv](https://docs.astral.sh/uv/) package manager

### Installing pgvector

```bash
# macOS (Homebrew)
brew install pgvector

# Ubuntu/Debian
sudo apt install postgresql-16-pgvector

# From source
cd /tmp
git clone --branch v0.8.0 https://github.com/pgvector/pgvector.git
cd pgvector
make && sudo make install
```

## Quick Start

```bash
# 1. Clone and enter the example directory
cd examples/02_python_pgvector_openai_example

# 2. Install dependencies
uv sync

# 3. Run the demo
uv run python run.py \
  "postgresql://admin:admin@127.0.0.1:5433/postgres" \
  "sk-your-openai-api-key"
```

Or using environment variables:

```bash
cp .env.example .env
# Edit .env with your DATABASE_URL and OPENAI_API_KEY
uv run python run.py
```

Or the one-liner script:

```bash
./run_demo.sh "postgresql://admin:admin@127.0.0.1:5433/postgres" "sk-..."
```

## How It Works

### 1. Enable pgvector

```sql
CREATE EXTENSION IF NOT EXISTS vector;
```

### 2. Create a Table with a Vector Column

```sql
CREATE TABLE documents (
    id        SERIAL PRIMARY KEY,
    content   TEXT NOT NULL,
    category  TEXT,
    embedding vector(1536)   -- 1536 dimensions for text-embedding-3-small
);
```

### 3. Generate Embeddings (Python)

```python
from openai import OpenAI

client = OpenAI(api_key="sk-...")
response = client.embeddings.create(
    input="PostgreSQL is a powerful database",
    model="text-embedding-3-small",
)
embedding = response.data[0].embedding  # list of 1536 floats
```

### 4. Insert with Embedding

```sql
INSERT INTO documents (content, category, embedding)
VALUES ('PostgreSQL is a powerful database', 'tech', '[0.1, 0.2, ...]');
```

### 5. Similarity Search

```sql
-- Cosine distance (lower = more similar)
SELECT content, embedding <=> '[0.1, 0.2, ...]' AS distance
FROM   documents
ORDER  BY embedding <=> '[0.1, 0.2, ...]'
LIMIT  5;

-- L2 (Euclidean) distance
SELECT content, embedding <-> '[0.1, 0.2, ...]' AS distance
FROM   documents
ORDER  BY embedding <-> '[0.1, 0.2, ...]'
LIMIT  5;
```

### 6. Add an Index for Performance

```sql
-- HNSW index
CREATE INDEX ON documents
USING hnsw (embedding vector_cosine_ops);
```

## Project Structure

```
02_python_pgvector_openai_example/
├── pg_vector_search/        # Main package
│   ├── __init__.py
│   ├── database.py          # PostgreSQL + pgvector operations
│   ├── embeddings.py        # OpenAI embedding wrapper
│   └── main.py              # Demo runner with sample data
├── run.py                   # Entry point
├── run_demo.sh              # One-liner script
├── pyproject.toml           # Dependencies (uv)
├── .env.example             # Environment variable template
├── .python-version
├── .gitignore
└── README.md
```

## Distance Operators

| Operator | Distance Metric | Use Case |
|----------|----------------|----------|
| `<=>` | Cosine distance | Text similarity (direction matters, not magnitude) |
| `<->` | L2 (Euclidean) | When absolute distance matters |
| `<#>` | Inner product (negative) | When vectors are normalized |

## Embedding Models

| Model | Dimensions | Speed | Cost |
|-------|-----------|-------|------|
| `text-embedding-3-small` | 1536 | Fast | $0.02 / 1M tokens |
| `text-embedding-3-large` | 3072 | Slower | $0.13 / 1M tokens |
| `text-embedding-ada-002` | 1536 | Fast | $0.10 / 1M tokens (legacy) |

## Tips

- **Normalize your text** before embedding: trim whitespace, remove excessive newlines
- **Batch API calls**: `embed_texts()` is much faster than calling `embed_text()` in a loop
- **Choose the right index**: IVFFlat for < 1M rows, HNSW for larger datasets
- **Filter first, then search**: use `WHERE category = 'x'` to narrow before vector search
- **Cosine distance → similarity**: `similarity = 1 - cosine_distance`

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `extension "vector" is not available` | Install pgvector on your PostgreSQL server |
| `could not access file "vector"` | Restart PostgreSQL after installing pgvector |
| `openai.AuthenticationError` | Check your `OPENAI_API_KEY` |
| `dimension mismatch` | Make sure `vector(N)` matches your model's output dimensions |

## License

MIT

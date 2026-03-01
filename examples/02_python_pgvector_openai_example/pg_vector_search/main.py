"""
Main demo — semantic search with pgvector + OpenAI embeddings.

This script walks through the full workflow:
1. Connect to PostgreSQL & set up pgvector
2. Prepare sample documents
3. Generate embeddings via OpenAI
4. Store documents + embeddings
5. Run semantic similarity queries
6. Clean up
"""

import sys
import time
import logging

from .database import VectorDB
from .embeddings import Embedder

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s  %(levelname)-7s  %(message)s",
    datefmt="%H:%M:%S",
)
logger = logging.getLogger(__name__)

# ------------------------------------------------------------------
# Sample data — short documents across several topics
# ------------------------------------------------------------------
SAMPLE_DOCUMENTS = [
    # --- Programming ---
    {
        "content": "Python is a high-level programming language known for its simplicity and readability. It supports multiple paradigms including object-oriented and functional programming.",
        "category": "programming",
    },
    {
        "content": "Rust is a systems programming language focused on safety, speed, and concurrency. Its ownership model eliminates data races at compile time.",
        "category": "programming",
    },
    {
        "content": "JavaScript is the language of the web. With Node.js it can also run on the server side, enabling full-stack development with a single language.",
        "category": "programming",
    },
    # --- Databases ---
    {
        "content": "PostgreSQL is a powerful open-source relational database. It supports advanced features like JSONB, full-text search, and extensions such as pgvector for vector similarity search.",
        "category": "database",
    },
    {
        "content": "Redis is an in-memory key-value store often used as a cache or message broker. It supports data structures like strings, hashes, lists, and sorted sets.",
        "category": "database",
    },
    {
        "content": "TiKV is a distributed transactional key-value database built in Rust. It uses the Raft consensus algorithm and is a core component of TiDB.",
        "category": "database",
    },
    # --- AI / ML ---
    {
        "content": "Large language models like GPT-4 are trained on massive text corpora. They can generate human-like text, translate languages, and answer questions.",
        "category": "ai",
    },
    {
        "content": "Vector embeddings convert text into dense numerical representations. Similar meanings map to nearby points in high-dimensional space, enabling semantic search.",
        "category": "ai",
    },
    {
        "content": "RAG (Retrieval-Augmented Generation) combines a retriever that finds relevant documents with a generative model that synthesizes an answer from those documents.",
        "category": "ai",
    },
    # --- Cooking (different domain to show contrast) ---
    {
        "content": "To make a classic Italian carbonara, cook spaghetti al dente, then toss with a sauce of egg yolks, Pecorino Romano cheese, guanciale, and black pepper.",
        "category": "cooking",
    },
    {
        "content": "Sourdough bread requires a fermented starter, strong flour, water, and salt. The long fermentation develops complex flavors and a chewy crumb.",
        "category": "cooking",
    },
    {
        "content": "Green tea is made from unoxidized Camellia sinensis leaves. Brewing at 70-80 degrees Celsius for 2-3 minutes produces the best flavor without bitterness.",
        "category": "cooking",
    },
]

# Queries to run against the documents
SAMPLE_QUERIES = [
    "How do vector databases work?",
    "What programming language is best for beginners?",
    "Tell me about distributed storage systems",
    "How to make pasta at home?",
    "What is retrieval augmented generation?",
]


# ------------------------------------------------------------------
# Demo entry-point
# ------------------------------------------------------------------
def demo(dsn: str, api_key: str, model: str | None = None):
    """
    Run the full semantic-search demo.

    Args:
        dsn:     PostgreSQL connection string.
        api_key: OpenAI API key.
        model:   Optional embedding model override.
    """
    print("=" * 72)
    print("  Semantic Search Demo — pgvector + OpenAI Embeddings")
    print("=" * 72)

    # ---- 1. Embedder --------------------------------------------------
    embedder = Embedder(api_key=api_key, model=model or "text-embedding-3-small")
    dim = embedder.dimensions
    print(f"\n[1/6] Embedder ready  model={embedder.model}  dim={dim}")

    # ---- 2. Database --------------------------------------------------
    db = VectorDB(dsn=dsn, embedding_dim=dim)
    if not db.connect():
        print("ERROR: cannot connect to PostgreSQL — aborting.")
        sys.exit(1)
    print("[2/6] Connected to PostgreSQL")

    try:
        # ---- 3. Schema ------------------------------------------------
        db.drop_table()          # start fresh for the demo
        db.setup()
        print("[3/6] Table & index created")

        # ---- 4. Embed & insert ----------------------------------------
        print(f"[4/6] Generating embeddings for {len(SAMPLE_DOCUMENTS)} documents ...")
        t0 = time.time()

        contents = [d["content"] for d in SAMPLE_DOCUMENTS]
        embeddings = embedder.embed_texts(contents)

        docs_to_insert = []
        for doc, emb in zip(SAMPLE_DOCUMENTS, embeddings):
            docs_to_insert.append({
                "content": doc["content"],
                "category": doc["category"],
                "embedding": emb,
            })

        ids = db.insert_documents_batch(docs_to_insert)
        elapsed = time.time() - t0
        print(f"      Inserted {len(ids)} documents in {elapsed:.2f}s")
        print(f"      Document IDs: {ids}")

        # ---- 5. Similarity search -------------------------------------
        print(f"\n[5/6] Running {len(SAMPLE_QUERIES)} semantic queries ...\n")

        for i, query in enumerate(SAMPLE_QUERIES, 1):
            print(f"  Query {i}: \"{query}\"")
            print("  " + "-" * 60)

            query_emb = embedder.embed_text(query)
            results = db.search_similar(query_emb, top_k=3)

            for rank, row in enumerate(results, 1):
                sim = 1 - row["distance"]          # cosine distance → similarity
                cat = row["category"] or "—"
                snippet = row["content"][:90] + "..." if len(row["content"]) > 90 else row["content"]
                print(f"  #{rank}  similarity={sim:.4f}  [{cat}]  {snippet}")

            print()

        # ---- 5b. Filtered search (by category) -----------------------
        print("  Filtered search — category='database', query='distributed systems'")
        print("  " + "-" * 60)
        q_emb = embedder.embed_text("distributed systems")
        filtered = db.search_similar(q_emb, top_k=3, category="database")
        for rank, row in enumerate(filtered, 1):
            sim = 1 - row["distance"]
            snippet = row["content"][:90] + "..." if len(row["content"]) > 90 else row["content"]
            print(f"  #{rank}  similarity={sim:.4f}  [{row['category']}]  {snippet}")
        print()

        # ---- 5c. L2 distance search -----------------------------------
        print("  L2 (Euclidean) distance search — query='machine learning'")
        print("  " + "-" * 60)
        q_emb = embedder.embed_text("machine learning")
        l2_results = db.search_by_l2(q_emb, top_k=3)
        for rank, row in enumerate(l2_results, 1):
            snippet = row["content"][:90] + "..." if len(row["content"]) > 90 else row["content"]
            print(f"  #{rank}  L2_dist={row['distance']:.4f}  [{row['category']}]  {snippet}")
        print()

        # ---- 6. Stats & cleanup --------------------------------------
        print(f"[6/6] Total documents in table: {db.count_documents()}")

    finally:
        db.close()

    print("\n" + "=" * 72)
    print("  Demo complete!")
    print("=" * 72)
    print()
    print("What you learned:")
    print("  - CREATE EXTENSION vector           (enable pgvector)")
    print("  - vector(N) column type              (store embeddings)")
    print("  - <=> operator                       (cosine distance)")
    print("  - <-> operator                       (L2 / Euclidean distance)")
    print("  - HNSW index                         (approximate nearest neighbor)")
    print("  - OpenAI text-embedding-3-small API  (generate embeddings)")
    print("  - Semantic search vs. keyword search  (meaning, not exact match)")
    print("=" * 72)

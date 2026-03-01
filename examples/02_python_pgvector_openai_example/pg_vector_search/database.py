"""
Database connection and pgvector table setup.

Works with both standard PostgreSQL (+ pgvector extension) and db9-server
(which has built-in vector support without needing CREATE EXTENSION).
"""

import json
import psycopg2
import psycopg2.extras
import psycopg2.extensions
import logging

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


def _try_register_pgvector(conn) -> bool:
    """Try to register the pgvector psycopg2 adapter; return True on success."""
    try:
        from pgvector.psycopg2 import register_vector
        register_vector(conn)
        return True
    except Exception:
        return False


class _VectorAdapter:
    """psycopg2 adapter: converts a Python list of floats to a vector literal."""
    def __init__(self, values):
        self._values = values

    def getquoted(self):
        csv = ",".join(str(float(v)) for v in self._values)
        return f"'[{csv}]'::vector".encode()


def _adapt_list_as_vector(lst):
    return _VectorAdapter(lst)


class VectorDB:
    def __init__(self, dsn: str, embedding_dim: int = 1536):
        self.dsn = dsn
        self.embedding_dim = embedding_dim
        self.conn = None
        self._pgvector_registered = False

    def connect(self) -> bool:
        try:
            self.conn = psycopg2.connect(self.dsn)
            self.conn.autocommit = True

            self._pgvector_registered = _try_register_pgvector(self.conn)
            if self._pgvector_registered:
                logger.info("Connected (pgvector adapter registered)")
            else:
                # Fallback: teach psycopg2 to send Python lists as vector literals
                psycopg2.extensions.register_adapter(list, _adapt_list_as_vector)
                logger.info("Connected (using string vector adapter for db9-server)")
            return True
        except Exception as e:
            logger.error(f"Connection failed: {e}")
            return False

    def close(self):
        """Close the database connection."""
        if self.conn:
            self.conn.close()
            logger.info("Connection closed")

    # ------------------------------------------------------------------
    # Schema setup
    # ------------------------------------------------------------------
    def setup(self):
        with self.conn.cursor() as cur:
            # db9-server has built-in vector; standard PG needs the extension
            try:
                cur.execute("CREATE EXTENSION IF NOT EXISTS vector")
                logger.info("pgvector extension enabled")
            except Exception:
                logger.info("CREATE EXTENSION skipped (db9-server has built-in vector support)")

            cur.execute(f"""
                CREATE TABLE IF NOT EXISTS documents (
                    id        SERIAL PRIMARY KEY,
                    content   TEXT        NOT NULL,
                    category  TEXT,
                    embedding vector({self.embedding_dim})
                )
            """)
            logger.info("Table 'documents' ready")

            # HNSW index
            try:
                cur.execute("""
                    CREATE INDEX IF NOT EXISTS idx_documents_embedding
                    ON documents
                    USING hnsw (embedding vector_cosine_ops)
                """)
                logger.info("HNSW index created on embedding column")
            except Exception:
                logger.info("HNSW index skipped (falling back to sequential scan)")

    def drop_table(self):
        """Drop the documents table (for demo cleanup)."""
        with self.conn.cursor() as cur:
            cur.execute("DROP TABLE IF EXISTS documents")
            logger.info("Table 'documents' dropped")

    # ------------------------------------------------------------------
    # CRUD
    # ------------------------------------------------------------------
    def _vec(self, embedding: list[float]):
        """Convert embedding to the right wire format for the current backend."""
        if self._pgvector_registered:
            import numpy as np
            return np.array(embedding)
        return embedding  # _adapt_list_as_vector handles serialisation

    def insert_document(self, content: str, embedding: list[float],
                        category: str | None = None) -> int:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO documents (content, category, embedding) "
                "VALUES (%s, %s, %s) RETURNING id",
                (content, category, self._vec(embedding)),
            )
            return cur.fetchone()[0]

    def insert_documents_batch(self, docs: list[dict]) -> list[int]:
        ids = []
        with self.conn.cursor() as cur:
            for doc in docs:
                cur.execute(
                    "INSERT INTO documents (content, category, embedding) "
                    "VALUES (%s, %s, %s) RETURNING id",
                    (doc["content"], doc.get("category"),
                     self._vec(doc["embedding"])),
                )
                ids.append(cur.fetchone()[0])
        return ids

    # ------------------------------------------------------------------
    # Similarity search
    # ------------------------------------------------------------------
    def search_similar(
        self,
        query_embedding: list[float],
        top_k: int = 5,
        category: str | None = None,
    ) -> list[dict]:
        vec = self._vec(query_embedding)
        with self.conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
            if category:
                cur.execute(
                    "SELECT id, content, category, "
                    "       embedding <=> %s AS distance "
                    "FROM   documents "
                    "WHERE  category = %s "
                    "ORDER  BY embedding <=> %s "
                    "LIMIT  %s",
                    (vec, category, vec, top_k),
                )
            else:
                cur.execute(
                    "SELECT id, content, category, "
                    "       embedding <=> %s AS distance "
                    "FROM   documents "
                    "ORDER  BY embedding <=> %s "
                    "LIMIT  %s",
                    (vec, vec, top_k),
                )
            return [dict(r) for r in cur.fetchall()]

    def search_by_l2(
        self,
        query_embedding: list[float],
        top_k: int = 5,
    ) -> list[dict]:
        vec = self._vec(query_embedding)
        with self.conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
            cur.execute(
                "SELECT id, content, category, "
                "       embedding <-> %s AS distance "
                "FROM   documents "
                "ORDER  BY embedding <-> %s "
                "LIMIT  %s",
                (vec, vec, top_k),
            )
            return [dict(r) for r in cur.fetchall()]

    def count_documents(self) -> int:
        """Return the number of documents in the table."""
        with self.conn.cursor() as cur:
            cur.execute("SELECT COUNT(*) FROM documents")
            return cur.fetchone()[0]

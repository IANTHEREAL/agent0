"""
OpenAI embedding helper.

Wraps the OpenAI API to generate text embeddings for documents and queries.
"""

import logging
from openai import OpenAI

logger = logging.getLogger(__name__)

# Default model — small, fast, cheap, 1536 dimensions
DEFAULT_MODEL = "text-embedding-3-small"


class Embedder:
    """Thin wrapper around the OpenAI Embeddings API."""

    def __init__(
        self,
        api_key: str,
        model: str = DEFAULT_MODEL,
    ):
        """
        Args:
            api_key: OpenAI API key.
            model:   Embedding model name.
                     "text-embedding-3-small"  →  1536 dims  (default)
                     "text-embedding-3-large"  →  3072 dims
        """
        self.client = OpenAI(api_key=api_key)
        self.model = model
        logger.info(f"Embedder ready  model={model}")

    @property
    def dimensions(self) -> int:
        """Return the expected embedding dimension for the current model."""
        dims = {
            "text-embedding-3-small": 1536,
            "text-embedding-3-large": 3072,
            "text-embedding-ada-002": 1536,
        }
        return dims.get(self.model, 1536)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    def embed_text(self, text: str) -> list[float]:
        """
        Generate an embedding for a single text string.

        Args:
            text: The input text.

        Returns:
            A list of floats (the embedding vector).
        """
        response = self.client.embeddings.create(
            input=text,
            model=self.model,
        )
        return response.data[0].embedding

    def embed_texts(self, texts: list[str]) -> list[list[float]]:
        """
        Generate embeddings for multiple texts in one API call.

        OpenAI accepts up to ~8 k tokens per text and batches of up to
        2048 texts in a single request.

        Args:
            texts: List of input strings.

        Returns:
            List of embedding vectors (same order as input).
        """
        response = self.client.embeddings.create(
            input=texts,
            model=self.model,
        )
        # The API may return results in arbitrary order → sort by index
        sorted_data = sorted(response.data, key=lambda d: d.index)
        return [d.embedding for d in sorted_data]

"""Embedding and generation wrapper for OpenAI."""

from __future__ import annotations

from openai import OpenAI


class OpenAIClient:
    def __init__(self, *, api_key: str, embed_model: str, chat_model: str):
        self.client = OpenAI(api_key=api_key)
        self.embed_model = embed_model
        self.chat_model = chat_model

    def embed_texts(self, texts: list[str]) -> list[list[float]]:
        response = self.client.embeddings.create(input=texts, model=self.embed_model)
        items = sorted(response.data, key=lambda d: d.index)
        return [item.embedding for item in items]

    def embed_texts_batched(self, texts: list[str], *, batch_size: int = 128) -> list[list[float]]:
        if batch_size <= 0:
            raise ValueError("batch_size must be > 0")
        all_embeddings: list[list[float]] = []
        for i in range(0, len(texts), batch_size):
            batch = texts[i : i + batch_size]
            all_embeddings.extend(self.embed_texts(batch))
        return all_embeddings

    def embed_text(self, text: str) -> list[float]:
        response = self.client.embeddings.create(input=text, model=self.embed_model)
        return response.data[0].embedding

    def answer(self, *, question: str, context_chunks: list[str]) -> str:
        numbered_context = "\n\n".join(
            f"[{idx}] {chunk}" for idx, chunk in enumerate(context_chunks, start=1)
        )
        response = self.client.chat.completions.create(
            model=self.chat_model,
            temperature=0.2,
            messages=[
                {
                    "role": "system",
                    "content": (
                        "You are a precise RAG assistant. Answer only from provided context. "
                        "If context is insufficient, explicitly say what is missing."
                    ),
                },
                {
                    "role": "user",
                    "content": (
                        f"Question:\n{question}\n\n"
                        f"Context:\n{numbered_context}\n\n"
                        "Give a concise answer and cite context chunk ids like [1], [2]."
                    ),
                },
            ],
        )
        return response.choices[0].message.content or ""

"""Entry point for the pgvector + OpenAI semantic search demo."""

import os
import sys
from dotenv import load_dotenv

load_dotenv()


def main():
    dsn = os.getenv("DATABASE_URL") or (sys.argv[1] if len(sys.argv) > 1 else None)
    api_key = os.getenv("OPENAI_API_KEY") or (sys.argv[2] if len(sys.argv) > 2 else None)
    model = os.getenv("OPENAI_EMBEDDING_MODEL")

    if not dsn:
        print("Usage: python run.py <postgresql_dsn> [openai_api_key]")
        print("   or: set DATABASE_URL and OPENAI_API_KEY environment variables")
        print()
        print("Examples:")
        print("  python run.py postgresql://admin:admin@127.0.0.1:5433/postgres sk-...")
        print("  DATABASE_URL=postgresql://... OPENAI_API_KEY=sk-... python run.py")
        sys.exit(1)

    if not api_key:
        print("ERROR: OpenAI API key is required.")
        print("  Set OPENAI_API_KEY env var or pass as second argument.")
        sys.exit(1)

    from pg_vector_search.main import demo
    demo(dsn=dsn, api_key=api_key, model=model)


if __name__ == "__main__":
    main()

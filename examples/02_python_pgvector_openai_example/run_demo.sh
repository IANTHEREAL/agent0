#!/bin/bash
# Quick demo script — pgvector + OpenAI semantic search
#
# Usage:
#   ./run_demo.sh <postgresql_dsn> <openai_api_key>
#   ./run_demo.sh                              # uses .env or env vars

set -euo pipefail

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    echo "Usage: ./run_demo.sh [postgresql_dsn] [openai_api_key]"
    echo ""
    echo "Arguments (or set via environment / .env file):"
    echo "  postgresql_dsn   DATABASE_URL   e.g. postgresql://admin:admin@127.0.0.1:5433/postgres"
    echo "  openai_api_key   OPENAI_API_KEY e.g. sk-..."
    echo ""
    echo "Examples:"
    echo "  ./run_demo.sh postgresql://admin:admin@127.0.0.1:5433/postgres sk-..."
    echo "  export DATABASE_URL=postgresql://admin:admin@127.0.0.1:5433/postgres"
    echo "  export OPENAI_API_KEY=sk-..."
    echo "  ./run_demo.sh"
    exit 0
fi

# Override env vars if positional args are given
[ -n "${1:-}" ] && export DATABASE_URL="$1"
[ -n "${2:-}" ] && export OPENAI_API_KEY="$2"

echo "Installing dependencies with uv..."
uv sync

echo ""
echo "Running semantic search demo..."
uv run python run.py ${DATABASE_URL:-} ${OPENAI_API_KEY:-}

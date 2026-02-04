#!/bin/bash
# Quick demo script using uv

if [ -z "$1" ]; then
    echo "Usage: ./run_demo.sh <postgresql_dsn>"
    echo ""
    echo "Example:"
    echo "  ./run_demo.sh postgresql://localhost/todo_db"
    echo "  ./run_demo.sh postgresql://user:pass@localhost:5432/todo_db"
    echo ""
    echo "Or use environment variable:"
    echo "  export DATABASE_URL=postgresql://localhost/todo_db"
    echo "  ./run_demo.sh \$DATABASE_URL"
    exit 1
fi

echo "Installing dependencies with uv..."
uv sync

echo ""
echo "Running TODO app demo..."
uv run python run.py "$1"

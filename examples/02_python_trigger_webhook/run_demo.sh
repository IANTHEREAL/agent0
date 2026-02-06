#!/bin/bash
# Quick demo script using uv

if [ -z "$1" ]; then
    echo "Usage: ./run_demo.sh <postgresql_dsn> [webhook_host] [webhook_port]"
    echo ""
    echo "Example:"
    echo "  ./run_demo.sh postgresql://admin:admin@127.0.0.1:5433/postgres"
    echo "  ./run_demo.sh postgresql://admin:admin@127.0.0.1:5433/postgres 0.0.0.0 9000"
    echo ""
    echo "Prerequisites:"
    echo "  - TiKV running: tiup playground --mode tikv-slim"
    echo "  - TiPG running: PGTIKV_TRIGGER_ENABLED=true PGTIKV_HTTP_ALLOW_INSECURE=true cargo run"
    exit 1
fi

echo "Installing dependencies with uv..."
uv sync

echo ""
echo "Running trigger webhook demo..."
uv run python run.py "$@"

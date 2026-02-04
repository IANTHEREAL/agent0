#!/usr/bin/env bash

set -euo pipefail

if [[ -z "${PG_DSN:-}" ]]; then
  echo "PG_DSN is required (e.g. postgres://admin:admin@127.0.0.1:5433/postgres)" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
E2E_DIR="$REPO_ROOT/e2e/sqlalchemy_smoke"

cd "$E2E_DIR"
uv run --locked python -m sqlalchemy_smoke

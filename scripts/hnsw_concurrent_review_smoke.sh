#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/hnsw_concurrent_review_smoke.sh --dsn <postgres://...>
    [--iterations <n>] [--rows <n>] [--table <name>]

Environment variable fallbacks:
  PG_DSN       Connection string when --dsn is omitted
  ITERATIONS   Worker loop count (default: 120)
  ROWS         Seed row count (default: 300)
  TABLE        Table name (default: hnsw_conc_review)
USAGE
}

DSN="${PG_DSN:-}"
ITERATIONS="${ITERATIONS:-120}"
ROWS="${ROWS:-300}"
TABLE="${TABLE:-hnsw_conc_review}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dsn)
      DSN="${2:-}"
      shift 2
      ;;
    --iterations)
      ITERATIONS="${2:-}"
      shift 2
      ;;
    --rows)
      ROWS="${2:-}"
      shift 2
      ;;
    --table)
      TABLE="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$DSN" ]]; then
  echo "Missing DSN. Pass --dsn or set PG_DSN." >&2
  exit 2
fi

if ! [[ "$ITERATIONS" =~ ^[0-9]+$ ]] || ! [[ "$ROWS" =~ ^[0-9]+$ ]]; then
  echo "--iterations and --rows must be positive integers." >&2
  exit 2
fi

if ! [[ "$TABLE" =~ ^[a-zA-Z_][a-zA-Z0-9_]*$ ]]; then
  echo "--table must be a valid unquoted SQL identifier." >&2
  exit 2
fi

echo "[INFO] hnsw concurrent review smoke"
echo "[INFO] DSN: $DSN"
echo "[INFO] TABLE=$TABLE ITERATIONS=$ITERATIONS ROWS=$ROWS"

# PostgreSQL requires pgvector extension for VECTOR/HNSW.
# db9 may not support CREATE EXTENSION, so keep this best-effort.
psql "$DSN" -X -v ON_ERROR_STOP=0 -c "CREATE EXTENSION IF NOT EXISTS vector;" >/dev/null 2>&1 || true

psql "$DSN" -X -v ON_ERROR_STOP=1 <<SQL
DROP TABLE IF EXISTS ${TABLE};
CREATE TABLE ${TABLE}(
  id SERIAL PRIMARY KEY,
  note TEXT,
  embedding VECTOR(3)
);
INSERT INTO ${TABLE}(note, embedding)
SELECT
  'seed-' || g,
  format(
    '[%s,%s,%s]',
    (g % 13)::float / 13.0,
    ((g + 1) % 13)::float / 13.0,
    ((g + 2) % 13)::float / 13.0
  )::vector(3)
FROM generate_series(1, ${ROWS}) AS g;
CREATE INDEX idx_${TABLE}_hnsw ON ${TABLE} USING hnsw (embedding vector_l2_ops);
SQL

W1_FAIL_FILE="$(mktemp)"
W2_FAIL_FILE="$(mktemp)"
trap 'rm -f "$W1_FAIL_FILE" "$W2_FAIL_FILE"' EXIT

worker_non_vector() {
  local fails=0
  local i
  for i in $(seq 1 "$ITERATIONS"); do
    if ! psql "$DSN" -X -v ON_ERROR_STOP=1 -c \
      "UPDATE ${TABLE} SET note='nv-${i}' WHERE id BETWEEN 1 AND 250;" >/dev/null 2>&1; then
      fails=$((fails + 1))
    fi
  done
  echo "$fails" >"$W1_FAIL_FILE"
}

worker_vector() {
  local fails=0
  local i
  for i in $(seq 1 "$ITERATIONS"); do
    if ! psql "$DSN" -X -v ON_ERROR_STOP=1 -c \
      "UPDATE ${TABLE} SET embedding='[0.91,0.11,0.21]' WHERE id=1;" >/dev/null 2>&1; then
      fails=$((fails + 1))
    fi
  done
  echo "$fails" >"$W2_FAIL_FILE"
}

worker_non_vector & pid1=$!
worker_vector & pid2=$!
wait "$pid1"
wait "$pid2"

w1_fails="$(cat "$W1_FAIL_FILE")"
w2_fails="$(cat "$W2_FAIL_FILE")"

if [[ "$w1_fails" != "0" || "$w2_fails" != "0" ]]; then
  echo "[FAIL] concurrent workers had errors: w1=$w1_fails w2=$w2_fails" >&2
  exit 1
fi

if command -v pg_isready >/dev/null 2>&1; then
  if ! pg_isready -d "$DSN" >/dev/null 2>&1; then
    echo "[FAIL] pg_isready failed after concurrent workload." >&2
    exit 1
  fi
fi

row_count="$(psql "$DSN" -X -v ON_ERROR_STOP=1 -qAt -c "SELECT count(*) FROM ${TABLE};")"
if [[ "$row_count" != "$ROWS" ]]; then
  echo "[FAIL] row count mismatch: got=$row_count expected=$ROWS" >&2
  exit 1
fi

note_group_count="$(psql "$DSN" -X -v ON_ERROR_STOP=1 -qAt -c \
  "SELECT count(DISTINCT note) FROM ${TABLE} WHERE id BETWEEN 1 AND 250;")"
if [[ "$note_group_count" != "1" ]]; then
  echo "[FAIL] notes in id[1,250] not converged: distinct_notes=$note_group_count" >&2
  exit 1
fi

echo "[PASS] hnsw concurrent review smoke passed"
echo "[PASS] row_count=$row_count w1_fails=$w1_fails w2_fails=$w2_fails"

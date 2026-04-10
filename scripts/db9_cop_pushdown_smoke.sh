#!/usr/bin/env bash
set -euo pipefail

PGHOST="${PGHOST:-127.0.0.1}"
PGPORT="${PGPORT:-5433}"
PGDATABASE="${PGDATABASE:-postgres}"
SMOKE_PASSWORD="${PGPASSWORD:-admin}"
SMOKE_USER_OVERRIDE="${DB9_PUSHDOWN_SMOKE_USER:-${PGUSER:-}}"
DB9_SERVER_LOG="${DB9_SERVER_LOG:-/tmp/db9-server.log}"

PSQL_ARGS=(-X -A -t -v ON_ERROR_STOP=1 -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE")
AUTH_USER=""

log() {
  printf '[db9-cop-smoke] %s\n' "$*"
}

detect_auth_user() {
  local -a candidates=()
  declare -A seen=()

  if [[ -n "$SMOKE_USER_OVERRIDE" ]]; then
    candidates+=("$SMOKE_USER_OVERRIDE")
  fi
  candidates+=("default.admin" "admin")

  for user in "${candidates[@]}"; do
    [[ -n "$user" ]] || continue
    [[ -z "${seen[$user]:-}" ]] || continue
    seen["$user"]=1

    if PGPASSWORD="$SMOKE_PASSWORD" psql "${PSQL_ARGS[@]}" -U "$user" -c 'SELECT 1' >/dev/null 2>&1; then
      AUTH_USER="$user"
      return 0
    fi
  done

  return 1
}

psql_query() {
  local pushdown="$1"
  local sql="$2"

  PGPASSWORD="$SMOKE_PASSWORD" \
    PGOPTIONS="-c db9.enable_cop_pushdown=${pushdown}" \
    psql "${PSQL_ARGS[@]}" -F '|' -U "$AUTH_USER" -c "$sql"
}

psql_explain() {
  local pushdown="$1"
  local sql="$2"

  PGPASSWORD="$SMOKE_PASSWORD" \
    PGOPTIONS="-c db9.enable_cop_pushdown=${pushdown}" \
    psql "${PSQL_ARGS[@]}" -U "$AUTH_USER" -c "EXPLAIN ${sql}"
}

psql_explain_verbose() {
  local pushdown="$1"
  local sql="$2"

  PGPASSWORD="$SMOKE_PASSWORD" \
    PGOPTIONS="-c db9.enable_cop_pushdown=${pushdown}" \
    psql "${PSQL_ARGS[@]}" -U "$AUTH_USER" -c "EXPLAIN VERBOSE ${sql}"
}

seed_smoke_data() {
  log "seeding smoke tables"
  PGPASSWORD="$SMOKE_PASSWORD" psql -X -v ON_ERROR_STOP=1 -h "$PGHOST" -p "$PGPORT" -U "$AUTH_USER" -d "$PGDATABASE" <<'SQL'
DROP TABLE IF EXISTS db9_cop_smoke;
CREATE TABLE db9_cop_smoke(id INT PRIMARY KEY, v TEXT, n INT);
CREATE INDEX db9_cop_smoke_n_idx ON db9_cop_smoke(n);
INSERT INTO db9_cop_smoke VALUES
  (1, 'a', 10),
  (2, 'b', 20),
  (3, 'c', 30),
  (4, 'd', 40);

DROP TABLE IF EXISTS db9_cop_prefix_smoke;
CREATE TABLE db9_cop_prefix_smoke(id INT PRIMARY KEY, status TEXT, created_at INT, pad TEXT);
CREATE INDEX db9_cop_prefix_smoke_status_created_at_idx ON db9_cop_prefix_smoke(status, created_at);
INSERT INTO db9_cop_prefix_smoke VALUES
  (1, 'active', 100, 'aa'),
  (2, 'active', 200, 'bb'),
  (3, 'inactive', 150, 'cc'),
  (4, 'active', 300, 'dd');
SQL
}

assert_explain_contains_pushdown() {
  local name="$1"
  local sql="$2"
  local output

  log "EXPLAIN(on): ${name}"
  output="$(psql_explain on "$sql")"
  printf '%s\n' "$output"
  grep -F 'DB9 Cop' <<<"$output" >/dev/null
  if grep -F 'Task: cop[tikv]' <<<"$output" >/dev/null; then
    log "unexpected legacy task annotation for ${name}"
    exit 1
  fi
}

assert_explain_contains_all() {
  local name="$1"
  local sql="$2"
  shift 2
  local output
  local needle

  log "EXPLAIN VERBOSE(on): ${name}"
  output="$(psql_explain_verbose on "$sql")"
  printf '%s\n' "$output"
  grep -F 'DB9 Cop' <<<"$output" >/dev/null
  if grep -F 'Task: cop[tikv]' <<<"$output" >/dev/null; then
    log "unexpected legacy task annotation for ${name}"
    exit 1
  fi

  for needle in "$@"; do
    grep -F "$needle" <<<"$output" >/dev/null || {
      log "missing EXPLAIN detail for ${name}: ${needle}"
      exit 1
    }
  done
}

assert_explain_stays_local() {
  local name="$1"
  local pushdown="$2"
  local sql="$3"
  local output

  log "EXPLAIN(${pushdown}): ${name}"
  output="$(psql_explain "$pushdown" "$sql")"
  printf '%s\n' "$output"
  if grep -F 'DB9 Cop' <<<"$output" >/dev/null || grep -F 'Task: cop[tikv]' <<<"$output" >/dev/null; then
    log "unexpected cop annotation for ${name} with pushdown=${pushdown}"
    exit 1
  fi
}

assert_parity() {
  local name="$1"
  local sql="$2"
  local expected="$3"
  local on_output
  local off_output

  log "SELECT(on): ${name}"
  on_output="$(psql_query on "$sql")"
  printf '%s\n' "$on_output"

  log "SELECT(off): ${name}"
  off_output="$(psql_query off "$sql")"
  printf '%s\n' "$off_output"

  if [[ "$on_output" != "$off_output" ]]; then
    diff -u <(printf '%s\n' "$off_output") <(printf '%s\n' "$on_output") || true
    log "pushdown on/off result mismatch for ${name}"
    exit 1
  fi

  if [[ "$on_output" != "$expected" ]]; then
    log "unexpected result for ${name}"
    diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$on_output") || true
    exit 1
  fi
}

main() {
  if ! detect_auth_user; then
    log "failed to authenticate with the candidate users: ${SMOKE_USER_OVERRIDE:-<auto>} default.admin admin"
    exit 1
  fi

  log "auth_user=${AUTH_USER}"
  seed_smoke_data

  assert_explain_stays_local \
    "exact lookup local fallback" \
    off \
    "SELECT id, v FROM db9_cop_smoke WHERE n = 20 LIMIT 1;"

  assert_explain_contains_all \
    "exact lookup" \
    "SELECT id, v FROM db9_cop_smoke WHERE n = 20 LIMIT 1;" \
    "DB9 Cop Access: point (20)" \
    "DB9 Cop Output: id, v" \
    "DB9 Cop Limit: 1"
  assert_parity \
    "exact lookup" \
    "SELECT id, v FROM db9_cop_smoke WHERE n = 20 LIMIT 1;" \
    $'2|b'

  assert_explain_contains_all \
    "in-list lookup" \
    "SELECT id, v FROM db9_cop_smoke WHERE n IN (10, 30) ORDER BY id;" \
    "DB9 Cop Access: in-list (10), (30)"
  assert_parity \
    "in-list lookup" \
    "SELECT id, v FROM db9_cop_smoke WHERE n IN (10, 30) ORDER BY id;" \
    $'1|a\n3|c'

  assert_explain_contains_all \
    "bounded one-sided range" \
    "SELECT id, v FROM db9_cop_smoke WHERE n >= 20 LIMIT 1;" \
    "DB9 Cop Access: range [20, +inf)" \
    "DB9 Cop Filter: (n >= 20)" \
    "DB9 Cop Output: id, v" \
    "DB9 Cop Limit: 1"
  assert_parity \
    "bounded one-sided range" \
    "SELECT id, v FROM db9_cop_smoke WHERE n >= 20 LIMIT 1;" \
    $'2|b'

  assert_explain_contains_all \
    "bounded two-sided range" \
    "SELECT id, v FROM db9_cop_smoke WHERE n >= 15 AND n < 30 LIMIT 1;" \
    "DB9 Cop Access: range [15, 30)" \
    "DB9 Cop Filter: ((n >= 15) AND (n < 30))" \
    "DB9 Cop Output: id, v" \
    "DB9 Cop Limit: 1"
  assert_parity \
    "bounded two-sided range" \
    "SELECT id, v FROM db9_cop_smoke WHERE n >= 15 AND n < 30 LIMIT 1;" \
    $'2|b'

  assert_explain_contains_all \
    "composite prefix lookup" \
    "SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' ORDER BY created_at LIMIT 2;" \
    "DB9 Cop Access: prefix ('active')" \
    "DB9 Cop Filter: (status = 'active'::text)" \
    "Index Cond: (status = 'active'::text)"
  assert_parity \
    "composite prefix lookup" \
    "SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' ORDER BY created_at LIMIT 2;" \
    $'1|100\n2|200'

  assert_explain_contains_all \
    "composite prefix + bounded range" \
    "SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' AND created_at >= 200 ORDER BY created_at LIMIT 2;" \
    "DB9 Cop Access: prefix ('active'), range [200, +inf)" \
    "DB9 Cop Filter: ((status = 'active'::text) AND (created_at >= 200))" \
    "Index Cond: ((status = 'active'::text) AND (created_at >= 200))"
  assert_parity \
    "composite prefix + bounded range" \
    "SELECT id, created_at FROM db9_cop_prefix_smoke WHERE status = 'active' AND created_at >= 200 ORDER BY created_at LIMIT 2;" \
    $'2|200\n4|300'

  assert_explain_contains_pushdown \
    "function whitelist projection" \
    "SELECT lower(v), upper(v), length(v), char_length(v), character_length(v), abs(n), coalesce(NULL, v), nullif(v, 'z') FROM db9_cop_smoke WHERE n = 20 LIMIT 1;"
  assert_parity \
    "function whitelist projection" \
    "SELECT lower(v), upper(v), length(v), char_length(v), character_length(v), abs(n), coalesce(NULL, v), nullif(v, 'z') FROM db9_cop_smoke WHERE n = 20 LIMIT 1;" \
    $'b|B|1|1|1|20|b|b'

  assert_parity \
    "unsupported substr keeps correct local semantics" \
    "SELECT substr(v, 1, 1) FROM db9_cop_smoke WHERE n = 20 LIMIT 1;" \
    $'b'

  if [[ -f "$DB9_SERVER_LOG" ]]; then
    log "tailing ${DB9_SERVER_LOG}"
    tail -n 40 "$DB9_SERVER_LOG"
  fi

  log "DB9 Cop pushdown smoke passed"
}

main "$@"

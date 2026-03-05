#!/usr/bin/env bash
# setup.sh — Bootstrap the db9 e2e local environment from scratch.
#
# What this script does:
#   1. Validates prerequisites (docker, docker compose)
#   2. Generates .env if missing (prompts for FS9_REPO_PATH, auto-generates secrets)
#   3. Brings up all 6 services with --build
#   4. Waits for all health checks to pass
#   5. Runs smoke tests: direct SQL against db9-server + fs9 health
#
# Usage:
#   ./setup.sh                        # full setup + smoke test
#   ./setup.sh --skip-build           # skip docker rebuild (use cached images)
#   ./setup.sh --smoke-only           # only run smoke tests against a running stack
#   ./setup.sh --reset                # wipe volumes, rebuild everything from scratch
#   ./setup.sh --multi-tenant-test    # legacy flag; now unsupported without db9-backend
#   ./setup.sh --binary svc=/path     # inject local binary into container after start
#                                     # svc: db9-server, fs9-server, fs9-meta
#                                     # example: --binary db9-server=/target/release/db9-server
#                                     # multiple binaries: --binary a=/p1 --binary b=/p2

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# ── Colour output ────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
info()  { echo -e "${BLUE}[e2e]${NC} $*"; }
ok()    { echo -e "${GREEN}[e2e]${NC} $*"; }
warn()  { echo -e "${YELLOW}[e2e]${NC} $*"; }
die()   { echo -e "${RED}[e2e] ERROR:${NC} $*" >&2; exit 1; }

# ── Argument parsing ─────────────────────────────────────────────────────────
SKIP_BUILD=false
SMOKE_ONLY=false
RESET=false
MULTI_TENANT_TEST=false
# Binary injection: parallel arrays (bash 3.2 compatible — no associative arrays)
BINARY_INJECT_SVCS=()   # service names
BINARY_INJECT_PATHS=()  # corresponding local paths

for arg in "$@"; do
  case "$arg" in
    --skip-build)        SKIP_BUILD=true ;;
    --smoke-only)        SMOKE_ONLY=true ;;
    --reset)             RESET=true ;;
    --multi-tenant-test) MULTI_TENANT_TEST=true ;;
    --binary=*)
      pair="${arg#--binary=}"
      svc="${pair%%=*}"
      bpath="${pair#*=}"
      BINARY_INJECT_SVCS+=("$svc")
      BINARY_INJECT_PATHS+=("$bpath")
      ;;
    --binary)
      die "--binary requires a value: --binary=service=/path/to/binary"
      ;;
    --help|-h)
      sed -n '2,16p' "$0" | sed 's/^# //'
      exit 0
      ;;
    *) die "Unknown argument: $arg" ;;
  esac
done

# ── Prerequisites ────────────────────────────────────────────────────────────
check_prereqs() {
  info "Checking prerequisites..."
  command -v docker >/dev/null 2>&1 || die "docker not found. Install Docker Desktop."
  docker compose version >/dev/null 2>&1 || die "docker compose plugin not found."
  ok "Prerequisites OK"
}

# ── .env setup ───────────────────────────────────────────────────────────────
setup_env() {
  if [[ -f .env ]]; then
    info ".env already exists — using it."
    # Verify FS9_REPO_PATH is set and valid
    source .env 2>/dev/null || true
    if [[ -z "${FS9_REPO_PATH:-}" ]]; then
      die "FS9_REPO_PATH is empty in .env. Edit .env and set FS9_REPO_PATH=/path/to/fs9"
    fi
    if [[ ! -d "${FS9_REPO_PATH}" ]]; then
      die "FS9_REPO_PATH=${FS9_REPO_PATH} does not exist."
    fi
    return
  fi

  info "No .env found — creating one."

  # Prompt for fs9 repo path
  local default_fs9="${HOME}/fs9"
  printf "  fs9 repo path [${default_fs9}]: "
  read -r fs9_path
  fs9_path="${fs9_path:-$default_fs9}"
  [[ -d "$fs9_path" ]] || die "Directory not found: $fs9_path"

  # Auto-generate secrets
  local jwt_secret meta_key
  jwt_secret=$(openssl rand -hex 32 2>/dev/null || LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 64)
  meta_key=$(openssl rand -hex 16 2>/dev/null || LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 32)

  cat > .env <<EOF
FS9_REPO_PATH=${fs9_path}

# Shared secrets — auto-generated, do NOT change after first run
FS9_JWT_SECRET=${jwt_secret}
FS9_META_KEY=${meta_key}

# PostgreSQL credentials for e2e smoke tests
POSTGRES_USER=admin
POSTGRES_PASSWORD=admin

# Optional: comma-separated API keys; empty = no key required
DB9_API_KEYS=

# Log level for all Rust services
RUST_LOG=info
EOF
  ok ".env created (FS9_REPO_PATH=${fs9_path})"
}

# ── Pre-flight checks ────────────────────────────────────────────────────────
preflight_checks() {
  source .env 2>/dev/null || true
  local fs9="${FS9_REPO_PATH:-}"

  # 1. Ensure fs9 has .dockerignore (avoids sending target/ ~17GB as build context)
  if [[ -n "$fs9" && -d "$fs9" && ! -f "$fs9/.dockerignore" ]]; then
    info "Creating ${fs9}/.dockerignore (excludes target/ and .git/ from Docker context)"
    cat > "$fs9/.dockerignore" <<'IGNORE'
target/
.git/
*.swp
*.swo
IGNORE
    ok ".dockerignore created — build context will be much smaller."
  fi

  # 2. Ensure Dockerfile.server-e2e exists in fs9 repo
  if [[ -n "$fs9" && -d "$fs9" && ! -f "$fs9/docker/Dockerfile.server-e2e" ]]; then
    if [[ -f "$SCRIPT_DIR/Dockerfile.fs9-server" ]]; then
      info "Copying Dockerfile.fs9-server → ${fs9}/docker/Dockerfile.server-e2e"
      mkdir -p "$fs9/docker"
      cp "$SCRIPT_DIR/Dockerfile.fs9-server" "$fs9/docker/Dockerfile.server-e2e"
      ok "Dockerfile.server-e2e installed."
    else
      warn "fs9/docker/Dockerfile.server-e2e not found and no reference copy available."
      warn "The fs9-server build will fail. Copy a Dockerfile.server-e2e into ${fs9}/docker/."
    fi
  fi

  # 3. Check for host port conflicts
  local ports=(5433 9999)
  local names=("db9-server" "fs9-server")
  local conflicts=()

  for i in "${!ports[@]}"; do
    local port="${ports[$i]}"
    local svc="${names[$i]}"
    local pid
    pid=$(ss -tlnp 2>/dev/null | grep ":${port} " | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)
    if [[ -n "$pid" ]]; then
      local pname
      pname=$(ps -p "$pid" -o comm= 2>/dev/null || echo "unknown")
      conflicts+=("  port ${port} (${svc}) ← pid ${pid} (${pname})")
    fi
  done

  if [[ ${#conflicts[@]} -gt 0 ]]; then
    warn "Host port conflicts detected:"
    for c in "${conflicts[@]}"; do echo -e "  ${YELLOW}${c}${NC}"; done
    printf "  Kill conflicting processes and continue? [Y/n] "
    read -r answer
    if [[ "${answer:-Y}" =~ ^[Yy]?$ ]]; then
      for i in "${!ports[@]}"; do
        local port="${ports[$i]}"
        local pid
        pid=$(ss -tlnp 2>/dev/null | grep ":${port} " | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)
        if [[ -n "$pid" ]]; then
          info "  Killing pid ${pid} (port ${port})..."
          kill "$pid" 2>/dev/null || true
        fi
      done
      sleep 1
      ok "Conflicting processes killed."
    else
      die "Aborting. Stop the conflicting services manually and re-run."
    fi
  fi
}

# ── Reset volumes ────────────────────────────────────────────────────────────
reset_volumes() {
  warn "Resetting: stopping and removing all containers and volumes..."
  docker compose down -v --remove-orphans 2>/dev/null || true
  ok "Volumes wiped."
}

# ── Build and start ───────────────────────────────────────────────────────────
start_stack() {
  if $SKIP_BUILD; then
    info "Starting stack (no rebuild)..."
    docker compose up -d
  else
    info "Building and starting all services (this takes 5–15 min on first run)..."
    docker compose up -d --build
  fi
}

# ── Inject local binaries ────────────────────────────────────────────────────
# Returns the in-container binary path for a given service name.
container_bin_path() {
  case "$1" in
    db9-server)      echo "/app/db9-server" ;;
    fs9-server)   echo "/app/fs9-server" ;;
    fs9-meta)     echo "/app/fs9-meta" ;;
    *) echo "" ;;
  esac
}

inject_binaries() {
  if [[ ${#BINARY_INJECT_SVCS[@]} -eq 0 ]]; then
    return
  fi

  info "Injecting local binaries into containers..."
  local i
  for (( i=0; i<${#BINARY_INJECT_SVCS[@]}; i++ )); do
    local svc="${BINARY_INJECT_SVCS[$i]}"
    local local_path="${BINARY_INJECT_PATHS[$i]}"
    local container_path
    container_path="$(container_bin_path "$svc")"

    [[ -f "$local_path" ]] || die "Binary not found: $local_path"
    [[ -n "$container_path" ]] || die "Unknown service for binary injection: $svc (valid: db9-server, fs9-server, fs9-meta)"

    info "  Injecting ${svc}: ${local_path} → ${container_path}"
    docker compose cp "${local_path}" "${svc}:${container_path}"
    docker compose exec -T "$svc" chmod +x "$container_path"
    docker compose restart "$svc"
    ok "  ${svc} restarted with local binary"
  done

  # Re-wait for health after restarts
  wait_healthy
}

# ── Wait for all services ─────────────────────────────────────────────────────
wait_healthy() {
  local services=(postgres pd tikv db9-server fs9-meta fs9-server)
  local timeout=300  # seconds
  local start=$SECONDS

  info "Waiting for all services to become healthy (timeout ${timeout}s)..."

  while true; do
    local all_healthy=true
    local status_line=""

    for svc in "${services[@]}"; do
      local state
      state=$(docker compose ps --format '{{.Health}}' "$svc" 2>/dev/null | head -1)
      case "$state" in
        healthy)   status_line+="${GREEN}✓${NC}${svc} " ;;
        starting)  status_line+="${YELLOW}…${NC}${svc} "; all_healthy=false ;;
        unhealthy) status_line+="${RED}✗${NC}${svc} "; all_healthy=false ;;
        "")        status_line+="${YELLOW}?${NC}${svc} "; all_healthy=false ;;
        *)         status_line+="${YELLOW}${state}${NC}:${svc} "; all_healthy=false ;;
      esac
    done

    printf "\r  %b" "$status_line"

    if $all_healthy; then
      echo ""
      ok "All services healthy."
      return 0
    fi

    if (( SECONDS - start >= timeout )); then
      echo ""
      die "Timeout after ${timeout}s. Check: docker compose logs"
    fi

    sleep 3
  done
}

# ── Smoke tests ───────────────────────────────────────────────────────────────
run_smoke_tests() {
  info "Running smoke tests..."

  # --- 1. pg_isready ---
  info "  [1/3] pg_isready against db9-server"
  docker compose exec -T postgres env PGPASSWORD=admin \
    pg_isready -h db9-server -p 5433 -U admin -d postgres >/dev/null 2>&1 \
    || die "pg_isready failed against db9-server"
  ok "  db9-server accepts PostgreSQL connections"

  # --- 2. SQL ---
  info "  [2/3] psql SELECT 1"
  local sql_out
  sql_out=$(docker compose exec -T postgres env PGPASSWORD=admin \
    psql -h db9-server -p 5433 -U admin -d postgres -At -v ON_ERROR_STOP=1 \
      -c "SELECT 1 AS answer;" 2>&1) \
    || die "psql smoke test failed:\n${sql_out}"
  echo "$sql_out" | grep -qx "1" \
    || die "SELECT 1 did not return expected result:\n${sql_out}"
  ok "  SQL OK"

  # --- 3. fs9 health ---
  info "  [3/3] fs9-server health"
  docker compose exec -T fs9-server curl -sf http://localhost:9999/health >/dev/null 2>&1 \
    || die "fs9-server health check failed"
  ok "  fs9-server health OK"

  echo ""
  ok "All smoke tests passed."
}

# ── Multi-tenant isolation tests ──────────────────────────────────────────────
run_multi_tenant_tests() {
  die "--multi-tenant-test is unavailable after removing the in-repo cloud admin target; use db9-backend for control-plane isolation tests."
}

# ── Print service URLs ────────────────────────────────────────────────────────
print_summary() {
  echo ""
  echo "─────────────────────────────────────────────────────"
  echo " db9 e2e stack is running"
  echo "─────────────────────────────────────────────────────"
  echo "  db9-server (psql)   : postgresql://localhost:5433"
  echo "  fs9-server       : http://localhost:9999"
  echo "  fs9-meta         : http://localhost:9998"
  echo ""
  echo " Direct SQL smoke check:"
  echo "  docker compose exec -T postgres env PGPASSWORD=admin psql -h db9-server -p 5433 -U admin -d postgres -c 'SELECT 1'"
  echo ""
  echo " Inject a local binary (e.g. after cargo build):"
  echo "  ./setup.sh --skip-build --binary=db9-server=../../target/release/db9-server"
  echo ""
  echo " Useful commands:"
  echo "  docker compose logs -f            # tail all logs"
  echo "  docker compose logs -f fs9-server # tail fs9-server"
  echo "  docker compose down               # stop all services"
  echo "  docker compose down -v            # stop + wipe volumes"
  echo "─────────────────────────────────────────────────────"
}

# ── Main ──────────────────────────────────────────────────────────────────────
main() {
  check_prereqs

  if $SMOKE_ONLY; then
    run_smoke_tests
    if $MULTI_TENANT_TEST; then
      run_multi_tenant_tests
    fi
    exit 0
  fi

  setup_env
  preflight_checks

  if $RESET; then
    reset_volumes
  fi

  start_stack
  wait_healthy

  # Inject local binaries if requested (restarts affected services + re-waits healthy)
  inject_binaries

  run_smoke_tests

  if $MULTI_TENANT_TEST; then
    run_multi_tenant_tests
  fi

  print_summary
}

main

#!/bin/bash
# Dify Compatibility Test Script for pg-tikv
# This script starts Dify with pg-tikv as the database backend

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PG_TIKV_DIR="$(dirname "$SCRIPT_DIR")"
DIFY_DIR="$HOME/lab/dify/docker"

# Configuration
PG_TIKV_PORT=${PG_TIKV_PORT:-5433}
PG_TIKV_HOST=${PG_TIKV_HOST:-0.0.0.0}
PD_ENDPOINTS=${PD_ENDPOINTS:-127.0.0.1:46515}
DIFY_KEYSPACE=${DIFY_KEYSPACE:-dify}

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

usage() {
    cat << EOF
Usage: $0 <command>

Commands:
    start       Start pg-tikv and Dify
    stop        Stop Dify (pg-tikv keeps running)
    restart     Restart Dify
    logs        Show Dify API logs
    status      Show status of all services
    clean       Stop Dify and clean up volumes
    pgtikv      Start only pg-tikv (for manual Dify control)

Environment Variables:
    PG_TIKV_PORT    pg-tikv port (default: 5433)
    PG_TIKV_HOST    pg-tikv listen address (default: 0.0.0.0)
    PD_ENDPOINTS    TiKV PD endpoints (default: 127.0.0.1:46515)
    DIFY_KEYSPACE   Keyspace/tenant name (default: dify)

Examples:
    $0 start                    # Start everything
    $0 logs                     # Follow Dify API logs
    $0 stop && $0 clean         # Full cleanup
EOF
}

check_tikv_cluster() {
    log_info "Checking TiKV cluster..."
    if ! curl -s "http://${PD_ENDPOINTS}/pd/api/v1/health" > /dev/null 2>&1; then
        log_error "TiKV cluster not running at $PD_ENDPOINTS"
        log_info "Start a cluster with: cd $PG_TIKV_DIR && uv run scripts/tikv_admin.py start --name dify-test"
        exit 1
    fi
    log_info "TiKV cluster is healthy"
}

start_pgtikv() {
    log_info "Starting pg-tikv on ${PG_TIKV_HOST}:${PG_TIKV_PORT}..."
    
    # Check if already running
    if ss -tlnp 2>/dev/null | grep -q ":${PG_TIKV_PORT}.*pg-tikv"; then
        log_info "pg-tikv already running on port ${PG_TIKV_PORT}"
        return 0
    fi
    
    # Kill any existing pg-tikv on this port
    pkill -f "pg-tikv.*PG_PORT=${PG_TIKV_PORT}" 2>/dev/null || true
    
    # Start pg-tikv
    cd "$PG_TIKV_DIR"
    PD_ENDPOINTS="$PD_ENDPOINTS" \
    PG_PORT="$PG_TIKV_PORT" \
    PG_LISTEN_ADDR="$PG_TIKV_HOST" \
    ./target/release/pg-tikv > /tmp/pgtikv-dify.log 2>&1 &
    
    # Wait for startup
    sleep 2
    
    if ss -tlnp 2>/dev/null | grep -q ":${PG_TIKV_PORT}"; then
        log_info "pg-tikv started successfully"
    else
        log_error "pg-tikv failed to start. Check /tmp/pgtikv-dify.log"
        tail -20 /tmp/pgtikv-dify.log
        exit 1
    fi
}

setup_dify_env() {
    log_info "Configuring Dify to use pg-tikv..."
    
    cd "$DIFY_DIR"
    
    # Backup original .env if not already backed up
    if [ ! -f .env.original ]; then
        cp .env .env.original
        log_info "Backed up original .env to .env.original"
    fi
    
    # Create pg-tikv specific .env
    cp .env.original .env
    
    # Modify database settings for pg-tikv
    # Use host.docker.internal for Docker to access host machine
    sed -i "s|^DB_HOST=.*|DB_HOST=host.docker.internal|" .env
    sed -i "s|^DB_PORT=.*|DB_PORT=${PG_TIKV_PORT}|" .env
    sed -i "s|^DB_USERNAME=.*|DB_USERNAME=${DIFY_KEYSPACE}.admin|" .env
    sed -i "s|^DB_PASSWORD=.*|DB_PASSWORD=admin|" .env
    sed -i "s|^DB_DATABASE=.*|DB_DATABASE=postgres|" .env
    
    # Also update plugin daemon database
    sed -i "s|^DB_PLUGIN_DATABASE=.*|DB_PLUGIN_DATABASE=dify_plugin|" .env
    
    log_info "Dify .env configured for pg-tikv"
    log_info "  DB_HOST=host.docker.internal"
    log_info "  DB_PORT=${PG_TIKV_PORT}"
    log_info "  DB_USERNAME=${DIFY_KEYSPACE}.admin"
}

start_dify() {
    log_info "Starting Dify..."
    cd "$DIFY_DIR"
    
    # Add host.docker.internal mapping for Linux
    # On Linux, we need to add extra_hosts to docker-compose
    
    # Start only essential services (skip db_postgres since we use pg-tikv)
    docker compose up -d redis weaviate sandbox ssrf_proxy
    
    # Wait for redis
    log_info "Waiting for Redis..."
    sleep 3
    
    # Start API and worker services
    docker compose up -d api worker worker_beat web nginx plugin_daemon
    
    log_info "Dify services starting..."
    log_info "Check status with: $0 logs"
}

stop_dify() {
    log_info "Stopping Dify..."
    cd "$DIFY_DIR"
    docker compose down
    log_info "Dify stopped"
}

show_logs() {
    cd "$DIFY_DIR"
    docker compose logs -f api worker
}

show_status() {
    echo ""
    log_info "=== pg-tikv Status ==="
    if ss -tlnp 2>/dev/null | grep -q ":${PG_TIKV_PORT}.*pg-tikv"; then
        echo -e "pg-tikv: ${GREEN}Running${NC} on port ${PG_TIKV_PORT}"
    else
        echo -e "pg-tikv: ${RED}Not Running${NC}"
    fi
    
    echo ""
    log_info "=== TiKV Cluster Status ==="
    if curl -s "http://${PD_ENDPOINTS}/pd/api/v1/health" > /dev/null 2>&1; then
        echo -e "TiKV: ${GREEN}Healthy${NC} at ${PD_ENDPOINTS}"
    else
        echo -e "TiKV: ${RED}Not Available${NC}"
    fi
    
    echo ""
    log_info "=== Dify Containers ==="
    cd "$DIFY_DIR"
    docker compose ps 2>/dev/null || echo "Dify not running"
}

clean_dify() {
    log_info "Cleaning Dify..."
    cd "$DIFY_DIR"
    docker compose down -v
    
    # Restore original .env
    if [ -f .env.original ]; then
        cp .env.original .env
        log_info "Restored original .env"
    fi
    
    log_info "Dify cleaned"
}

# Main
case "${1:-}" in
    start)
        check_tikv_cluster
        start_pgtikv
        setup_dify_env
        start_dify
        log_info "Dify starting with pg-tikv backend"
        log_info "Access Dify at: http://localhost/install"
        ;;
    stop)
        stop_dify
        ;;
    restart)
        stop_dify
        sleep 2
        start_dify
        ;;
    logs)
        show_logs
        ;;
    status)
        show_status
        ;;
    clean)
        clean_dify
        ;;
    pgtikv)
        check_tikv_cluster
        start_pgtikv
        log_info "pg-tikv is ready at ${PG_TIKV_HOST}:${PG_TIKV_PORT}"
        log_info "Connect with: psql -h 127.0.0.1 -p ${PG_TIKV_PORT} -U ${DIFY_KEYSPACE}.admin"
        ;;
    *)
        usage
        exit 1
        ;;
esac

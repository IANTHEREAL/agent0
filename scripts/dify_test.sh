#!/bin/bash
# Dify Compatibility Test Script for db9-server
# This script starts Dify with db9-server as the database backend

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DB9_DIR="$(dirname "$SCRIPT_DIR")"
DIFY_DIR="$HOME/lab/dify/docker"

# Configuration
DB9_PORT=${DB9_PORT:-5433}
DB9_HOST=${DB9_HOST:-0.0.0.0}
PD_ENDPOINTS=${PD_ENDPOINTS:-127.0.0.1:46515}
DIFY_KEYSPACE=${DIFY_KEYSPACE:-dify}
DB9_ADMIN_PASSWORD=${DB9_ADMIN_PASSWORD:-admin}

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
    start       Start db9-server and Dify
    stop        Stop Dify (db9-server keeps running)
    restart     Restart Dify
    logs        Show Dify API logs
    status      Show status of all services
    clean       Stop Dify and clean up volumes
    db9      Start only db9-server (for manual Dify control)

Environment Variables:
    DB9_PORT    db9-server port (default: 5433)
    DB9_HOST    db9-server listen address (default: 0.0.0.0)
    PD_ENDPOINTS    TiKV PD endpoints (default: 127.0.0.1:46515)
    DIFY_KEYSPACE   Keyspace/tenant name (default: dify)
    DB9_ADMIN_PASSWORD  Bootstrap/admin password for db9-server (default: admin)

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
        log_info "Start a cluster with: cd $DB9_DIR && uv run scripts/tikv_admin.py start --name dify-test"
        exit 1
    fi
    log_info "TiKV cluster is healthy"
}

start_db9() {
    log_info "Starting db9-server on ${DB9_HOST}:${DB9_PORT}..."
    
    # Check if already running
    if ss -tlnp 2>/dev/null | grep -q ":${DB9_PORT}.*db9-server"; then
        log_info "db9-server already running on port ${DB9_PORT}"
        return 0
    fi
    
    # Kill any existing db9-server on this port
    pkill -f "db9-server.*PG_PORT=${DB9_PORT}" 2>/dev/null || true
    
    # Start db9-server
    cd "$DB9_DIR"
    PD_ENDPOINTS="$PD_ENDPOINTS" \
    PG_PORT="$DB9_PORT" \
    PG_LISTEN_ADDR="$DB9_HOST" \
    PG_KEYSPACE="$DIFY_KEYSPACE" \
    DB9_BOOTSTRAP_ADMIN_USER=admin \
    DB9_BOOTSTRAP_ADMIN_PASSWORD="$DB9_ADMIN_PASSWORD" \
    DB9_INSECURE=1 \
    ./target/release/db9-server > /tmp/db9-dify.log 2>&1 &
    
    # Wait for startup
    sleep 2
    
    if ss -tlnp 2>/dev/null | grep -q ":${DB9_PORT}"; then
        log_info "db9-server started successfully"
    else
        log_error "db9-server failed to start. Check /tmp/db9-dify.log"
        tail -20 /tmp/db9-dify.log
        exit 1
    fi
}

setup_dify_env() {
    log_info "Configuring Dify to use db9-server..."
    
    cd "$DIFY_DIR"
    
    # Backup original .env if not already backed up
    if [ ! -f .env.original ]; then
        cp .env .env.original
        log_info "Backed up original .env to .env.original"
    fi
    
    # Create db9-server specific .env
    cp .env.original .env
    
    # Modify database settings for db9-server
    # Use host.docker.internal for Docker to access host machine
    sed -i "s|^DB_HOST=.*|DB_HOST=host.docker.internal|" .env
    sed -i "s|^DB_PORT=.*|DB_PORT=${DB9_PORT}|" .env
    sed -i "s|^DB_USERNAME=.*|DB_USERNAME=${DIFY_KEYSPACE}.admin|" .env
    sed -i "s|^DB_PASSWORD=.*|DB_PASSWORD=${DB9_ADMIN_PASSWORD}|" .env
    sed -i "s|^DB_DATABASE=.*|DB_DATABASE=postgres|" .env
    
    # Also update plugin daemon database
    sed -i "s|^DB_PLUGIN_DATABASE=.*|DB_PLUGIN_DATABASE=dify_plugin|" .env
    
    log_info "Dify .env configured for db9-server"
    log_info "  DB_HOST=host.docker.internal"
    log_info "  DB_PORT=${DB9_PORT}"
    log_info "  DB_USERNAME=${DIFY_KEYSPACE}.admin"
}

start_dify() {
    log_info "Starting Dify..."
    cd "$DIFY_DIR"
    
    # Add host.docker.internal mapping for Linux
    # On Linux, we need to add extra_hosts to docker-compose
    
    # Start only essential services (skip db_postgres since we use db9-server)
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
    log_info "=== db9-server Status ==="
    if ss -tlnp 2>/dev/null | grep -q ":${DB9_PORT}.*db9-server"; then
        echo -e "db9-server: ${GREEN}Running${NC} on port ${DB9_PORT}"
    else
        echo -e "db9-server: ${RED}Not Running${NC}"
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
        start_db9
        setup_dify_env
        start_dify
        log_info "Dify starting with db9-server backend"
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
    db9)
        check_tikv_cluster
        start_db9
        log_info "db9-server is ready at ${DB9_HOST}:${DB9_PORT}"
        log_info "Connect with: psql -h 127.0.0.1 -p ${DB9_PORT} -U ${DIFY_KEYSPACE}.admin"
        ;;
    *)
        usage
        exit 1
        ;;
esac

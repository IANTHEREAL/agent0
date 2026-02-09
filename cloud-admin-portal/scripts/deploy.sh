#!/bin/bash
# Deployment script

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

usage() {
    echo "Usage: $0 [command]"
    echo ""
    echo "Commands:"
    echo "  start     Start production services"
    echo "  stop      Stop production services"
    echo "  restart   Restart production services"
    echo "  status    Show service status"
    echo "  logs      Show service logs"
    echo "  build     Build and start services"
    echo ""
}

cd "$PROJECT_DIR/deploy"

case "${1:-start}" in
    start)
        echo -e "${GREEN}Starting services...${NC}"
        docker compose up -d
        echo -e "${GREEN}Services started${NC}"
        docker compose ps
        ;;
    stop)
        echo -e "${YELLOW}Stopping services...${NC}"
        docker compose down
        echo -e "${GREEN}Services stopped${NC}"
        ;;
    restart)
        echo -e "${YELLOW}Restarting services...${NC}"
        docker compose restart
        echo -e "${GREEN}Services restarted${NC}"
        docker compose ps
        ;;
    status)
        docker compose ps
        ;;
    logs)
        docker compose logs -f "${2:-}"
        ;;
    build)
        echo -e "${YELLOW}Building and starting services...${NC}"
        "$SCRIPT_DIR/build.sh"
        docker compose up -d
        echo -e "${GREEN}Services started${NC}"
        docker compose ps
        ;;
    *)
        usage
        exit 1
        ;;
esac

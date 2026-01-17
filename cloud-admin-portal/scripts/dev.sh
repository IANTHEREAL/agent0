#!/bin/bash
# Development startup script
# Starts both backend and frontend in development mode

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Configuration (can be overridden by env vars)
BACKEND_PORT="${PGTIKV_API_PORT:-8090}"
FRONTEND_PORT="${FRONTEND_PORT:-5173}"
PD_ENDPOINTS="${PD_ENDPOINTS:-127.0.0.1:2379}"
PG_HOST="${PGTIKV_PG_HOST:-127.0.0.1}"
PG_PORT="${PGTIKV_PG_PORT:-5433}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}pg-tikv Cloud Admin Portal - Development Mode${NC}"
echo "================================================"

# Check prerequisites
command -v uv >/dev/null 2>&1 || { echo -e "${RED}uv required but not found. Install with: curl -LsSf https://astral.sh/uv/install.sh | sh${NC}"; exit 1; }
command -v node >/dev/null 2>&1 || { echo -e "${RED}Node.js required but not found${NC}"; exit 1; }

# Check if ports are available
check_port() {
    if lsof -Pi :$1 -sTCP:LISTEN -t >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

if ! check_port $BACKEND_PORT; then
    echo -e "${YELLOW}Warning: Port $BACKEND_PORT is in use. Trying alternative...${NC}"
    BACKEND_PORT=$((BACKEND_PORT + 1))
    if ! check_port $BACKEND_PORT; then
        echo -e "${RED}Error: Port $BACKEND_PORT also in use. Please free the port or set PGTIKV_API_PORT${NC}"
        exit 1
    fi
fi

# Install backend dependencies if needed
cd "$PROJECT_DIR/backend"
if [ ! -f "uv.lock" ]; then
    echo -e "${YELLOW}Installing backend dependencies with uv...${NC}"
    uv sync
fi

# Install frontend dependencies if needed
if [ ! -d "$PROJECT_DIR/frontend/node_modules" ]; then
    echo -e "${YELLOW}Installing frontend dependencies...${NC}"
    cd "$PROJECT_DIR/frontend"
    npm install
fi

# Start backend in background
echo -e "${GREEN}Starting backend on http://localhost:$BACKEND_PORT${NC}"
echo -e "${YELLOW}PD Endpoints: $PD_ENDPOINTS${NC}"
echo -e "${YELLOW}pg-tikv: $PG_HOST:$PG_PORT${NC}"
cd "$PROJECT_DIR/backend"
PGTIKV_DEBUG=true PGTIKV_API_PORT=$BACKEND_PORT PD_ENDPOINTS=$PD_ENDPOINTS PGTIKV_PG_HOST=$PG_HOST PGTIKV_PG_PORT=$PG_PORT uv run uvicorn app.main:app --host 0.0.0.0 --port $BACKEND_PORT --reload &
BACKEND_PID=$!

# Wait for backend to start
sleep 2

# Start frontend with backend URL configured
echo -e "${GREEN}Starting frontend on http://localhost:$FRONTEND_PORT${NC}"
cd "$PROJECT_DIR/frontend"
VITE_BACKEND_URL="http://localhost:$BACKEND_PORT" npm run dev &
FRONTEND_PID=$!

# Trap to kill both processes on exit
trap "kill $BACKEND_PID $FRONTEND_PID 2>/dev/null" EXIT

echo ""
echo -e "${GREEN}Development servers started:${NC}"
echo "  Backend API:  http://localhost:$BACKEND_PORT/api"
echo "  API Docs:     http://localhost:$BACKEND_PORT/api/docs"
echo "  Frontend:     http://localhost:$FRONTEND_PORT"
echo "  Login:        password is 'admin'"
echo ""
echo "Press Ctrl+C to stop all servers"

# Wait for any process to exit
wait

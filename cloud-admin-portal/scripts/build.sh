#!/bin/bash
# Build script for production deployment

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}pg-tikv Cloud Admin Portal - Production Build${NC}"
echo "================================================"

# Build Rust backend
echo -e "${YELLOW}Building backend (release mode)...${NC}"
cd "$PROJECT_DIR/backend"

command -v cargo >/dev/null 2>&1 || { echo -e "${RED}cargo required but not found. Install Rust: https://rustup.rs${NC}"; exit 1; }

cargo build --release

echo -e "${GREEN}Backend binaries built:${NC}"
echo "  pgtikv-admin: $PROJECT_DIR/backend/target/release/pgtikv-admin"
echo "  pgtikv-ctl:   $PROJECT_DIR/backend/target/release/pgtikv-ctl"

# Build frontend
echo -e "${YELLOW}Building frontend...${NC}"
cd "$PROJECT_DIR/frontend"

if [ ! -d "node_modules" ]; then
    echo "Installing dependencies..."
    npm install
fi

npm run build

echo -e "${GREEN}Frontend built to frontend/dist/${NC}"

# Build Docker images (if docker is available)
if command -v docker >/dev/null 2>&1; then
    echo -e "${YELLOW}Building Docker images...${NC}"
    cd "$PROJECT_DIR/deploy"
    docker compose build
fi

echo ""
echo -e "${GREEN}Build complete!${NC}"
echo ""
echo "To run locally:"
echo "  ./backend/target/release/pgtikv-admin"
echo ""
echo "To start Docker services:"
echo "  cd deploy && docker compose up -d"

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

# Build frontend
echo -e "${YELLOW}Building frontend...${NC}"
cd "$PROJECT_DIR/frontend"

if [ ! -d "node_modules" ]; then
    echo "Installing dependencies..."
    npm install
fi

npm run build

echo -e "${GREEN}Frontend built to frontend/dist/${NC}"

# Build Docker images
echo -e "${YELLOW}Building Docker images...${NC}"
cd "$PROJECT_DIR/deploy"

docker-compose build

echo ""
echo -e "${GREEN}Build complete!${NC}"
echo ""
echo "To start the production services:"
echo "  cd deploy && docker-compose up -d"

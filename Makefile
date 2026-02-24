# db9-server Makefile
# PostgreSQL-compatible distributed SQL database on TiKV

.PHONY: all build release test unit-test integration-test orm-test \
        run dev clean fmt lint check \
        tikv-start tikv-stop tikv-clean tikv-list \
        full-test regression help

# Default target
all: build

# ============================================================================
# Build
# ============================================================================

build:
	cargo build

release:
	cargo build --release

clean:
	cargo clean
	rm -rf test-reports/

# ============================================================================
# Run
# ============================================================================

# Run in debug mode (requires TiKV running)
run:
	PD_ENDPOINTS=$${PD_ENDPOINTS:-127.0.0.1:2379} \
	PG_PORT=$${PG_PORT:-5433} \
	cargo run

# Run in release mode (requires TiKV running)
run-release:
	PD_ENDPOINTS=$${PD_ENDPOINTS:-127.0.0.1:2379} \
	PG_PORT=$${PG_PORT:-5433} \
	cargo run --release

# ============================================================================
# Test
# ============================================================================

# Unit tests only
unit-test:
	cargo test

test: unit-test

# Integration tests (requires running db9-server)
integration-test:
	python3 scripts/integration_test.py --dsn "$${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}"

# ORM tests (requires running db9-server)
orm-test:
	cd orm-tests && \
	[ -d node_modules ] || npm install --silent && \
	PG_DSN="$${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}" npm test

# Fast regression gate (<5min)
regression:
	bash scripts/regression_gate.sh

# Full automated test suite (starts TiKV + db9-server)
full-test:
	./run_tests.sh

# ============================================================================
# Code Quality
# ============================================================================

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

lint:
	cargo clippy -- -D warnings

check:
	cargo check

# ============================================================================
# TiKV Cluster Management
# ============================================================================

CLUSTER_NAME ?= dev

tikv-start:
	uv run scripts/tikv_admin.py start --name $(CLUSTER_NAME) --persistent

tikv-stop:
	uv run scripts/tikv_admin.py stop --name $(CLUSTER_NAME)

tikv-clean:
	uv run scripts/tikv_admin.py clean --name $(CLUSTER_NAME)

tikv-list:
	uv run scripts/tikv_admin.py list

# ============================================================================
# Development Workflow
# ============================================================================

# Start dev environment: TiKV cluster + db9-server
dev: tikv-start
	@echo "TiKV cluster '$(CLUSTER_NAME)' started"
	@echo "Run 'make run' or 'make run-release' in another terminal"

# Quick dev cycle: format, check, test
quick: fmt check unit-test

# ============================================================================
# Help
# ============================================================================

help:
	@echo "db9-server Makefile"
	@echo ""
	@echo "Build:"
	@echo "  make build        - Build debug binary"
	@echo "  make release      - Build release binary"
	@echo "  make clean        - Clean build artifacts"
	@echo ""
	@echo "Run (requires TiKV):"
	@echo "  make run          - Run in debug mode"
	@echo "  make run-release  - Run in release mode"
	@echo ""
	@echo "Test:"
	@echo "  make test         - Run unit tests"
	@echo "  make integration-test - Run integration tests (requires db9-server)"
	@echo "  make orm-test     - Run ORM tests (requires db9-server)"
	@echo "  make regression   - Fast regression gate (<5min)"
	@echo "  make full-test    - Full test suite (starts TiKV + db9-server)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make fmt          - Format code"
	@echo "  make fmt-check    - Check formatting"
	@echo "  make lint         - Run clippy"
	@echo "  make check        - Run cargo check"
	@echo "  make quick        - Format + check + unit test"
	@echo ""
	@echo "TiKV Cluster (CLUSTER_NAME=$(CLUSTER_NAME)):"
	@echo "  make tikv-start   - Start persistent TiKV cluster"
	@echo "  make tikv-stop    - Stop TiKV cluster"
	@echo "  make tikv-clean   - Clean TiKV cluster data"
	@echo "  make tikv-list    - List all clusters"
	@echo "  make dev          - Start TiKV for development"
	@echo ""
	@echo "Environment Variables:"
	@echo "  PD_ENDPOINTS      - TiKV PD address (default: 127.0.0.1:2379)"
	@echo "  PG_PORT           - db9-server listen port (default: 5433)"
	@echo "  PG_DSN            - PostgreSQL DSN for tests"
	@echo "  CLUSTER_NAME      - TiKV cluster name (default: dev)"

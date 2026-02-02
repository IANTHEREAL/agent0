SHELL := /usr/bin/env bash

GO ?= go
BIN_DIR := $(CURDIR)/bin

GOLANGCI_LINT_VERSION ?= v1.62.0
GOIMPORTS_VERSION ?= v0.26.0
GOVULNCHECK_VERSION ?= v1.1.3

GOLANGCI_LINT := $(BIN_DIR)/golangci-lint
GOIMPORTS := $(BIN_DIR)/goimports
GOVULNCHECK := $(BIN_DIR)/govulncheck

.PHONY: help
help: ## Show common targets
	@printf "agent0 development targets:\\n\\n"
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z0-9_\\-]+:.*##/ {printf "  \\033[36m%-18s\\033[0m %s\\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: tools
tools: ## Install pinned dev tools into ./bin
	@mkdir -p "$(BIN_DIR)"
	@echo "Installing tools into $(BIN_DIR)"
	@GOBIN="$(BIN_DIR)" $(GO) install github.com/golangci/golangci-lint/cmd/golangci-lint@$(GOLANGCI_LINT_VERSION)
	@GOBIN="$(BIN_DIR)" $(GO) install golang.org/x/tools/cmd/goimports@$(GOIMPORTS_VERSION)
	@GOBIN="$(BIN_DIR)" $(GO) install golang.org/x/vuln/cmd/govulncheck@$(GOVULNCHECK_VERSION)
	@echo "Tools installed:"
	@ls -1 "$(BIN_DIR)" | sed 's/^/  - /'

.PHONY: fmt
fmt: ## Format Go code (gofmt + goimports if installed)
	@$(GO) fmt ./...
	@if [[ -x "$(GOIMPORTS)" ]]; then \
		echo "Running goimports"; \
		"$(GOIMPORTS)" -w $$(find . -name '*.go' -not -path './bin/*'); \
	else \
		echo "goimports not installed (run: make tools)"; \
	fi

.PHONY: lint
lint: ## Run linters (golangci-lint)
	@if [[ ! -x "$(GOLANGCI_LINT)" ]]; then \
		echo "golangci-lint not installed (run: make tools)"; \
		exit 2; \
	fi
	"$(GOLANGCI_LINT)" run ./...

.PHONY: test
test: ## Run unit tests
	@$(GO) test ./...

.PHONY: build
build: ## Build agent0 binary to ./bin/agent0
	@mkdir -p "$(BIN_DIR)"
	@$(GO) build -o "$(BIN_DIR)/agent0" ./cmd/agent0

.PHONY: run
run: ## Run agent0 (loads .env if present). Pass args via RUN_ARGS=...
	@bash -c 'set -euo pipefail; set -a; [[ -f .env ]] && source .env; set +a; exec $(GO) run ./cmd/agent0 $${RUN_ARGS:-"--help"}'

.PHONY: vuln
vuln: ## Run govulncheck (requires make tools)
	@if [[ ! -x "$(GOVULNCHECK)" ]]; then \
		echo "govulncheck not installed (run: make tools)"; \
		exit 2; \
	fi
	"$(GOVULNCHECK)" ./...

.PHONY: tidy
tidy: ## Run go mod tidy
	@$(GO) mod tidy

.PHONY: clean
clean: ## Remove local build artifacts
	@rm -rf "$(BIN_DIR)"


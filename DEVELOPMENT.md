# Development

This repo is a small Go (`go 1.22`) module that runs an MVP Pantheon controller over MCP (JSON-RPC over HTTP/SSE).

## Quickstart

```bash
./setup_dev.sh
make test
make build
```

## Prerequisites

- Go `>= 1.22` (`go env GOVERSION`)
- `git`
- `make`

The setup script installs additional tools (linters/formatters) into `./bin` without requiring sudo.

## Setup

1) Install dev tooling and run a quick verification:

```bash
./setup_dev.sh
```

2) Configure runtime env vars:

```bash
cp .env.example .env
```

Edit `.env` and fill in at least:

- `PANTHEON_PROJECT_NAME`
- `PANTHEON_PARENT_BRANCH_ID` (first run only)
- `MCP_BASE_URL` (if not using `http://localhost:8000/mcp/sse`)

## Common tasks

- `make fmt` — format code
- `make lint` — run `golangci-lint` (uses `.golangci.yml`)
- `make test` — run unit tests
- `make build` — build `./bin/agent0`
- `make run` — run `go run` (loads `.env` if present)
- `make vuln` — run `govulncheck` (requires `make tools`)

Run `make help` for the full list.

## Running agent0

The CLI supports flags and environment variables (see `.env.example`).

### First run

The controller persists state to `./.agent0/controller_state.json`. If that file does not exist yet, the controller needs a parent branch ID to start from.

Example (flags):

```bash
go run ./cmd/agent0 \
  --mcp-base-url http://localhost:8000/mcp/sse \
  --pantheon-project-name agent0 \
  --pantheon-parent-branch-id <baseline_parent_branch_id> \
  --task "..."
```

Example (using `.env` + Makefile):

```bash
make run RUN_ARGS='--task "..."'
```

### Resume

If `./.agent0/controller_state.json` exists, `agent0` resumes from it:

```bash
go run ./cmd/agent0 --task "..."
```

## Architecture overview

High-level flow:

- `cmd/agent0/main.go` parses flags/env and calls `runtime/pantheon_client.RunController`.
- `runtime/pantheon_client/controller.go`:
  - Loads controller state (`./.agent0/controller_state.json` by default).
  - On first run, performs a bootstrap episode to install `AGENTS.md`/skills and register Minibook credentials.
  - Runs an episode loop:
    - calls `parallel_explore` to create a branch
    - polls branch status to a terminal state
    - reads `branch_output(full=true)` as the MVP success signal
    - promotes `anchor_branch_id` to the latest successful branch
- `runtime/pantheon_client/mcp.go` implements the MCP client (JSON-RPC over HTTP, supports SSE responses).

Local state and artifacts:

- `./.agent0/controller_state.json` is the only persisted state. It stores:
  - MCP base URL, project name, agent name
  - bootstrap status + branch IDs (`anchor_branch_id`, `active_episode_branch_id`)

## Contribution guidelines

- Keep changes small and focused; prefer explicit error handling and clear contracts.
- Run `make fmt && make lint && make test` before sending changes.
- Avoid committing secrets:
  - do not commit `.env` (use `.env.example` as the template)
  - do not commit `./.agent0/*` (already ignored)
- Branch naming: use descriptive prefixes (e.g. `pantheon/feat-...`).
- Commit messages: short, imperative, scoped when helpful (e.g. `chore: add dev tooling`).


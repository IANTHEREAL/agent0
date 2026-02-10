# agent0 (MVP Pantheon controller)

Runs an infinite (or bounded) episode loop on Pantheon via MCP:

- 1 episode = 1 `parallel_explore(num_branches=1)` producing 1 branch.
- `anchor_branch_id` advances to the latest successful branch.
- If the branch status is `failed`, retry up to 3 times (sleep 20 minutes between retries) then exit.
- On first run (local state not initialized), a bootstrap episode runs first to install `AGENTS.md`/`skills` and add the MCP server via `codex mcp add` (or force it via `--rebootstrap`).

## Usage

First run (needs a baseline parent branch):

```bash
go run ./cmd/agent0 \
  --mcp-base-url http://localhost:8000/mcp/sse \
  --mcp-bearer-token <token> \
  --pantheon-project-name agent0 \
  --pantheon-parent-branch-id <baseline_parent_branch_id> \
  --task "..."
```

Resume (reads controller state from `./.agent0/controller_state.json`):

```bash
go run ./cmd/agent0 --task "..."
```

Optional initialization hints:

- `--agents-md-url <url>`
- `--skills-url <url>`
- `--project-collaboration-md-url <url>`
- `--minibook-account <account>`
- `--rebootstrap`

Optional MCP auth:

- `--mcp-bearer-token <token>` (or `MCP_BEARER_TOKEN` / `PANTHEON_BEARER_TOKEN`)

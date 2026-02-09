# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

See `AGENTS.md` for full knowledge base (structure, where-to-look, environment variables, key patterns).

## Quick Reference

```bash
# Backend (Rust)
cd backend && cargo check               # Type check
cd backend && cargo build --release     # Build both binaries

# Frontend (React + TypeScript)
cd frontend && npm install
cd frontend && npx tsc --noEmit         # Type check
cd frontend && npm run dev              # Dev server (port 5173)
cd frontend && npm run build            # Production build

# Full dev stack
./scripts/dev.sh                        # Backend + frontend together
```

**URLs:** Frontend http://localhost:5173 | API http://localhost:8090/api

## Critical Notes

- **Backend is Rust (axum)**, NOT Python. `backend/app/` is dead legacy code (only .pyc artifacts).
- Never reference `backend-rs/` — Rust backend lives at `backend/` directly.
- Never use `as any`, `@ts-ignore` in frontend or `unsafe` in Rust.
- Never use `unwrap()`/`expect()` in handler code — use `?` with `AppError`.
- Never use sqlx compile-time macros (`query!`, `query_as!`) — incompatible with AnyPool.

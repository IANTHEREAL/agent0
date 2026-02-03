# PRFAQ: Testing Hardening (Fast Regression Gate)

**Date**: 2026-02-03  
**Scope**: Test strategy + regression gate + retro/preventions  

## Press Release (future tense)

Today we are introducing a **Fast Regression Gate** for pg-tikv: a single command that developers and CI can run to quickly validate release-critical compatibility regressions with a stable evidence trail.

This change targets:
- Faster, safer iteration on `master` without relying on release ceremony
- Reduced SSOT drift (no more “the gate referenced a test file that doesn’t exist”)
- Clear, reproducible failure evidence (logs + minimal repro SQL/ORM paths)

**What we ship**
- A one-command fast gate: `bash scripts/regression_gate.sh`
- A manifest file as SSOT for the gate pack: `scripts/regression_gate.list`
- A CI workflow that runs the fast gate on every PR/push and can be configured as a required check

**What success looks like**
- PR authors can run the gate locally and get a pass/fail in <5 minutes (typical)
- CI reports a single, deterministic status for the fast gate on every PR
- Any change to the gate pack is visible and reviewable (via the manifest)

## FAQ

### How do I run the fast gate?

```bash
# default: start TiKV + pg-tikv, then run the gate pack
bash scripts/regression_gate.sh
```

Reuse an existing running pg-tikv instance:

```bash
bash scripts/regression_gate.sh --dsn "$PG_DSN"
```

Skip ORM pack (SQL-only):

```bash
bash scripts/regression_gate.sh --skip-orm
```

### How do I run the full suite?

```bash
./run_tests.sh
```

The full suite is slower but covers a wider surface area (integration + broad ORM pack).

### What is included in the fast gate?

The SSOT is `scripts/regression_gate.list`:
- `[sql]` entries run via `scripts/integration_test.py`
- `[orm]` entries run via `npm test` under `orm-tests/` (Vitest filters)

### What is explicitly excluded (and why)?

- **Outbound-internet dependent tests** (e.g. `tests/91_http_extension.sql`) are not suitable for a “fast + deterministic” gate. Keep them in the full suite or make them hermetic (local mock server) before promoting into the fast gate.

### How do we add a new regression case?

**Rule**: “No PR = not done.” A regression case is only accepted if it is merged and reproducible by others/CI.

For SQL regressions:
1) Add `tests/NNN_name.sql`
2) Add either `tests/NNN_name.expected` (exact output) or `tests/NNN_name.assert` (subset match)
3) Ensure it runs via `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/NNN_name.sql`
4) Add it to `scripts/regression_gate.list` (if it must be in the fast gate)

For ORM regressions:
1) Add/adjust a minimal test case under `orm-tests/`
2) Make it runnable as a focused filter (keep it fast)
3) Add it to the `[orm]` section in `scripts/regression_gate.list`

### What do we do in restricted networks?

- Prefer the fast gate pack that does not rely on external services.
- For CI TiUP mirror flake, add caching/retry and/or prefetch components; avoid relying on human reruns as the primary mitigation.

## Retro / Repeated Failures (and preventions)

### 1) SSOT drift (gate referenced missing files)

**Symptom**: A Stage Packet/DoD required a test file that did not exist on the referenced RC SHA (e.g. `tests/125_*`).  
**Root cause**: The “gate definition” lived in prose; there was no enforced, versioned SSOT.  
**Prevention**:
- Gate definition must live in-repo as a manifest (`scripts/regression_gate.list`)
- CI runs the gate on every PR; missing files fail fast
**Verification**: CI status + manifest diff in PR review.

Related: #329, #335

### 2) Local-only changes claimed as “done”

**Symptom**: Work was reported complete but not reproducible by others/CI.  
**Root cause**: Missing “PR-as-evidence” enforcement.  
**Prevention**:
- “No PR = not done” (explicitly enforced in process)
- Required checks gate merges; avoid relying on manual memory of what to run
**Verification**: Every claimed completion includes an issue/PR link + CI status.

### 3) CI flakes (TiUP mirror timeouts, external dependencies)

**Symptom**: CI intermittently fails due to TiUP mirror/network instability (#297) or outbound dependencies (http extension).  
**Root cause**: Non-hermetic dependencies in the critical path.  
**Prevention**:
- Add caching/retry/prefetch for TiUP on CI
- Keep outbound-dependent tests out of the fast gate until made hermetic
**Verification**: Reduced flake rate over 1 week; fast gate remains stable.

Related: #297, #333

### 4) Permission/token bottlenecks for release operations

**Symptom**: Branch/tag/release operations get blocked when a single token/account fails.  
**Root cause**: Unclear ownership + lack of documented runbook/backup executor.  
**Prevention**:
- Document the “who can do what” runbook (tag/release/attachments)
- Ensure there is a backup executor for release actions
**Verification**: A dry-run path exists and is periodically exercised.


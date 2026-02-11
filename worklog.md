# Dead Code Detection — Worklog

## 2026-02-11

### Task
Systematic dead code detection across TiPG codebase (221 files, ~97K lines).

### Approach
7-phase analysis executed in parallel where possible:
1. Compiler `dead_code` lint — added `#![warn(dead_code)]` to main.rs, ran `cargo check`
2. Clippy dead-code-adjacent lints — `unnecessary_wraps`, `unused_self`, `redundant_clone`, `unused_async`
3. `#[allow(dead_code)]` audit — categorized all 74 annotations
4. Unreachable code patterns — `unreachable!()`, `todo!()`, `_ => {}`, dead-after-return
5. Unused dependencies — checked all 35 deps against actual usage
6. `pub` visibility over-exposure — counted pub vs pub(crate) items
7. Consolidated report

### Key Findings
- **3 compiler dead_code warnings** (unsuppressed) — gin.rs functions + FsBackend::exists()
- **74 `#[allow(dead_code)]` annotations** — 33 keep, 36 wire-in, 5 remove
- **402 clippy warnings** (project code) — 73 unnecessary_wraps, 19 unused_self, 11 redundant_clone
- **0 unused dependencies** — all 35 deps are actively used
- **618 over-exposed `pub` items** (73% of all visibility-annotated items)
- **~440 lines safely removable** without feature impact
- **~3,500 additional lines removable** pending roadmap decisions (type inference, RBAC, operator framework)

### Decisions
- Report produced as `dead_code_report.md` — categorized by phase with dispositions
- No code changes made in this pass (detection only)
- Recommended: tighten `pub` → `pub(crate)` as highest-leverage ongoing improvement

### Verification Deep-Dives (4 parallel investigations)

Verified every `#[allow(dead_code)]` item against actual call sites. Major corrections:

- **11 misleading annotations found** — items marked dead but actively used in production:
  - `GinTokens::iter_hashes()`, `into_scan_hashes()` — called in gin.rs scan path
  - `FsBackend::exists()` — implemented and tested
  - `list_materialized_views()` — called in drop_schema()
  - `replace_procedure()` — called in CREATE OR REPLACE PROCEDURE
  - `Executor::auth_manager()` — 6 call sites in default_privileges.rs
  - `VirtualTable::schema_name()` — 41 implementations
  - `find_keyword_outside_strings()` — called in parser.rs
  - `Session::session_user()` — called in dispatch.rs
  - `NestedLoopJoinOperator` — production join execution
  - `IndexEqScanOperator::new_with_scan_limit()` — production planner

- **RBAC module: 60% integrated** — DDL/auth/metadata fully wired, enforcement layer (`check_privilege()`) completely missing
- **Type inference: 90% integrated** — core module in production (62 call sites), only convenience APIs unwired
- **Operator framework: 70% integrated** — joins + scans active, EXPLAIN introspection methods test-only
- **Truly dead code reduced to ~433 lines** (down from initial ~440 estimate, but with different items)
- **Truly abandoned code: ~335 lines** (OperatorBuilder + extract_tsquery_gin_tokens)

### Deliverable
- `dead_code_report.md` — verified categorized report with corrected dispositions

---

# Issue #633: COPY TO STDOUT FORMAT binary silently treated as text; option chars lossy-cast

## 2026-02-11

### Root Cause Analysis

**Problem 1: Unsupported FORMAT silently ignored**
- Location: `src/protocol/copy_format.rs:48-61` (`CopyOptions::from_copy_options`)
- The FORMAT option only checks for `"CSV"` and sets `CopyFormat::Csv`. All other values — including `"BINARY"`, `"FOO"` — fall through silently and keep the default `CopyFormat::Text`.
- PostgreSQL behavior: `COPY ... WITH (FORMAT binary)` uses a completely different binary wire protocol. Silently downgrading to text breaks clients expecting binary frames.

**Problem 2: Delimiter/quote/escape chars lossy-cast via `as u8`**
- Location: `src/protocol/copy_format.rs:62-75`
- `char as u8` truncates to the low byte of the Unicode scalar value (e.g., `'€'` U+20AC → `0xAC`).
- PostgreSQL requires these to be single-byte characters and rejects multi-byte.

### Fix Plan
1. Change `from_copy_options` return type to `Result<Self, String>` for validation errors.
2. Validate FORMAT: accept TEXT/CSV, reject BINARY (not supported), reject unknown.
3. Validate delimiter/quote/escape: require `is_ascii()`, reject non-ASCII.
4. Update caller at `dynamic.rs:619` to propagate error via ErrorInfo.
5. Add tests for new validation paths.

### Error Codes (PostgreSQL-compatible)
- `0A000` (feature_not_supported) for all COPY option errors (matches PostgreSQL)

---

# Issues #630, #631, #632: COPY format encoding fixes

## 2026-02-11

### #630 (P0): CSV NULL vs empty string conflation
- **Root cause**: `encode_csv_value` only quoted values containing delimiter/quote/newline/CR. Empty string and custom NULL sentinel were never force-quoted.
- **Fix**: Added `tmp == opts.null_string.as_bytes()` to `needs_quote` condition in `encode_csv_value`.
- **Also fixed**: `encode_value_raw` for `Value::Bytes` — was falling through to `encode_value` which emits `\\x` (text-mode double escaping). Now emits `\x` directly.

### #631 (P1): Text mode custom delimiter/NULL not round-trippable
- **Root cause 1**: `escape_text` was hardcoded to escape only TAB/newline/CR/backslash. Custom delimiters (e.g. `|`) in values were not escaped.
- **Root cause 2**: Non-NULL values matching null_string were output identically to NULL.
- **Fix 1**: Added `delimiter` parameter to `escape_text` and `encode_value`. Custom delimiter byte is now escaped with backslash prefix.
- **Fix 2**: Post-encoding check in text mode: if encoded bytes match null_string, insert backslash before first byte. On import, `\X` unescapes to `X`, recovering the original value.

### #632 (P1): HEADER row bypasses format encoding
- **Root cause**: `handle_copy_to_stdout` emitted header by raw-concatenating column names with delimiter. Column names with delimiter/quote/newline produced invalid output.
- **Fix**: Wrap column names as `Value::Text` and route through `encode_row_with_options`.

### Verification
- 1064 tests pass (27 copy_format tests including 16 new). Zero regressions.

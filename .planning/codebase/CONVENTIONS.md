# Coding Conventions

**Analysis Date:** 2026-03-17

## Naming Patterns

**Files:**
- Rust modules use `snake_case` (e.g., `hash_semi_join.rs`, `logical_planner.rs`, `key_encoding.rs`)
- Modules too large for a single file become directories with `mod.rs` (e.g., `src/sql/operators/hash_join/`)
- Test-only files are named `tests.rs` and placed in the same directory as the module they test (e.g., `src/sql/analyzer/tests.rs`, `src/sql/operators/hash_join/tests.rs`)

**Types (structs, enums, traits):**
- `PascalCase` throughout (e.g., `TableScanOperator`, `AnalyzerError`, `MockCatalog`, `PhysicalOperator`)
- Operator types follow the pattern `{Name}Operator` (e.g., `HashSemiJoinOperator`, `TableScanOperator`)
- Error enums are named `{Layer}Error` (e.g., `AnalyzerError`, `SqlError`, `CatalogError`)

**Functions and methods:**
- `snake_case` throughout
- Constructors follow Rust idioms: `new()`, `new_with_{qualifier}()` (e.g., `new_with_scan_limit`, `new_with_rate_limit`, `new_with_limits`, `new_with_rows`)
- Test-only constructors are gated with `#[cfg(test)]` and use `new_stub()` for minimal stubs (e.g., `TikvStore::new_stub()`)
- Builder pattern used where construction has many optional fields: `MockCatalog::builder().table(...).build()`

**Variables and fields:**
- `snake_case` throughout (e.g., `scan_limit`, `table_id`, `pk_indices`)

**Constants:**
- `SCREAMING_SNAKE_CASE` (e.g., `OPERATOR_BATCH_FETCH_SIZE`, `GIN_BATCH_FETCH_SIZE`, `IMPLICIT_PK_TYPE`, `TAG_NULL`)

**Type aliases:**
- `PascalCase` matching their semantic role (e.g., `pub type BoxedOperator = Box<dyn PhysicalOperator>`)

## Code Style

**Formatting:**
- `rustfmt` is enforced via `cargo fmt -- --check` in CI (`.github/workflows/ci.yml`, line 42)
- No custom `rustfmt.toml` found — standard Rust formatting defaults apply

**Linting:**
- `cargo clippy --workspace --all-targets -- -D warnings` is enforced in CI
- `clippy.toml` sets `too-many-arguments-threshold = 14` (default is usually 7 — relaxed for complex SQL operations)
- Per-site `#[allow(clippy::too_many_arguments)]` used for DDL functions like `src/sql/ddl/create_index.rs`
- `#![allow(clippy::uninlined_format_args)]` at crate root in `src/main.rs`
- `#[allow(clippy::large_enum_variant)]` used for IR enum types in `src/sql/analyzer/types/mod.rs`

**Visibility:**
- `pub(crate)` used extensively (1,231+ uses in `src/sql/` alone) — internal APIs are not `pub` unless needed for cross-crate use
- `pub(super)` used for module-internal helpers (e.g., `pub(super) const IMPLICIT_PK_TYPE`)
- Structs, trait implementations, and methods prefer the minimum necessary visibility

## Import Organization

**Order (within a file):**
1. Standard library (`std::`) and external crates in alphabetical order
2. Crate-internal imports (`use crate::`) — model, sql, storage layers
3. Local module imports (`use super::`) and local re-exports

**Example from `src/sql/operators/scan.rs`:**
```rust
use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::sql::projection::fill_row_defaults;
```

**Example from `src/sql/analyzer/expr/mod.rs`:**
```rust
use sqlparser::ast::{self as ast, BinaryOperator, Expr, TrimWhereField};

use crate::model::{DataType, Value};
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::{common_type, comparison_target_type, unify_types};

use super::error::AnalyzerError;
use super::types::*;
use super::Analyzer;

use coercion::extract_array_literal_elems;
```

**Path Aliases:**
- No path aliases (`use X as Y`) except for `sqlparser::ast::{self as ast, ...}` to shorten AST access
- Wildcard `use super::*` is used exclusively inside `#[cfg(test)] mod tests` blocks

## Error Handling

**Strategy:** Two-tier error system — typed SQL errors with SQLSTATE codes at the SQL layer, `anyhow::Result` throughout for propagation.

**`anyhow!()` macro:** Used ubiquitously (2,215+ call sites) for ad-hoc errors in infrastructure, storage, and non-SQL layers. Migrate toward `SqlError` variants where SQLSTATE codes are needed.

**`SqlError` enum** (`src/sql/error.rs`): Structured errors for all SQL-layer failures. Uses `thiserror::Error` derive. Carries SQLSTATE codes. Converts from `AnalyzerError` via `impl From<AnalyzerError> for SqlError`.

**`AnalyzerError` enum** (`src/sql/analyzer/error.rs`): Fine-grained typed errors from the semantic analysis phase. Does NOT derive `thiserror` — implements `Debug + Clone` and manual `Display`. SQLSTATE codes are documented in variant comments (e.g., `// SQLSTATE 42P18`).

**`bail!` / `ensure!` macros:** Not used — `return Err(anyhow!(...))` or `return Err(SqlError::...)` patterns are used instead.

**Error propagation:** The `?` operator is used throughout. Errors from one layer convert to another via `From` implementations or `.map_err(|e| ...)` at conversion boundaries.

**Silent fallback prohibition:** The `scripts/lint_error_masking.sh` script bans `unwrap_or(DataType::Text)` without an `// INTENTIONAL:` comment (with rationale) on the same or preceding lines. Example:
```rust
// INTENTIONAL: empty array defaults element type to Text (PG-compatible)
.unwrap_or(DataType::Text)
```

## Logging

**Framework:** `tracing` crate — `tracing::info!`, `tracing::warn!`, `tracing::debug!`, `tracing::error!`, `tracing::trace!` macros (281 call sites in `src/`)

**Patterns:**
- `info!` for lifecycle events (tenant creation, eviction)
- `warn!` for recoverable failures and unexpected states that don't abort (e.g., S3 upload failures in `src/extensions/fs/embedded/pagefs.rs`)
- `debug!` for non-critical operational messages (e.g., reaper eviction counts)
- `error!` for unrecoverable failures
- Tracing setup is in `src/observability.rs`

## Comments

**Module-level doc comments:**
- Every public module starts with a `//!` doc comment block describing purpose, layer responsibilities, and (for complex modules) ASCII-art diagrams of data flow or phase sequences
- Example from `src/sql/operators/mod.rs`:
```rust
//! Physical operators for Volcano-style query execution
//!
//! # Architecture
//! ```text
//! SQL Query → Planner builds operator tree → ...
//! ```
```

**Item-level doc comments:**
- `///` doc comments on all public traits, structs, and their methods
- Trait method docs describe invariants (e.g., "After returning `None`, subsequent calls should also return `None`")

**Inline comments:**
- Used for non-obvious invariants, invariant identifiers (e.g., `// Invariant I3`), and attribution (e.g., `// Fast path: ...`)

**`#[allow]` justification tags (enforced by `scripts/lint_dead_code.sh`):**
- `#[allow(dead_code)]` must be single-line and carry one of four tags:
  - `// forward-compat:` — API surface kept for future use
  - `// serde:` — field used by serialization framework
  - `// framework:` — trait/API surface required by interface contract
  - `// test:` — fields accessed only from test code

## Function Design

**Size:** No explicit line limit, but large modules are split into sub-modules (e.g., `dynamic.rs` → `dynamic/` with `mod.rs`, `query.rs`, `copy.rs`, `startup.rs`)

**Parameters:** `clippy.toml` threshold of 14. Above 14, use `#[allow(clippy::too_many_arguments)]` with justification.

**Return Values:** `Result<T>` (using `anyhow::Result`) for fallible operations. `Option<T>` for lookups that may return nothing. Never raw panics in production paths.

**Async functions:** `async fn` used throughout. For trait methods, `#[async_trait]` from the `async-trait` crate is required (e.g., `PhysicalOperator` trait in `src/sql/operators/mod.rs`).

## Module Design

**Exports:**
- Large modules use `pub use submodule::*` in `mod.rs` to flatten the public API (e.g., `src/sql/operators/mod.rs` re-exports all operator types)
- `#[allow(unused_imports)]` with `// framework:` or `// Operator framework —` comments used for re-exported types not yet fully consumed downstream

**Barrel Files:**
- Used at module boundaries to flatten internal structure. See `src/sql/operators/mod.rs`, `src/sql/analyzer/mod.rs`.

**Module documentation pattern (`AGENTS.md`, `sot/README.md`):**
- Complex modules have accompanying `AGENTS.md` files for navigation (e.g., `src/sql/AGENTS.md`)
- `docs/sot/README.md` is the normative Source of Truth module registry

---

*Convention analysis: 2026-03-17*

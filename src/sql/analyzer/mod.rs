//! Unified Analyzer: single-pass name resolution + type checking.
//!
//! The Analyzer transforms raw `sqlparser::ast` into tipg's own Typed IR
//! (`TypedExpr`, `AnalyzedQuery`). Every expression node in the output carries
//! its resolved `DataType`, column references are positional indices, and all
//! syntax sugar is normalized to canonical forms.
//!
//! # Design
//!
//! - **Single-pass** (PostgreSQL `transformExpr` model): name resolution and
//!   type checking happen simultaneously during one recursive walk.
//! - **Sync**: the Analyzer uses a pre-fetched `CatalogSnapshot` — no async.
//! - **Scope chain**: nested scopes for subqueries, with `scope_depth` on
//!   `ColumnRef` replacing `SubstituteVisitor` for correlated subqueries.
//!
//! # Module structure
//!
//! - `types` — Typed IR definitions (`TypedExpr`, `TypedExprKind`, etc.)
//! - `error` — `AnalyzerError` type
//! - `scope` — Scope chain for column resolution
//! - `catalog` — `Catalog` trait and `CatalogSnapshot` implementation
//! - `expr` — Expression analysis (`analyze_expr`)
//! - `query` — Query-level analysis (`analyze_query`)

pub mod catalog;
pub mod error;
mod expr;
mod literal;
mod query;
pub mod scope;
pub mod types;

#[cfg(test)]
mod tests;

pub use catalog::{Catalog, CatalogSnapshot};
pub use error::AnalyzerError;
pub use scope::{Scope, ScopeStack};
pub use types::*;

/// The Analyzer: transforms raw SQL AST into Typed IR.
///
/// Maintains a scope stack for column resolution across nested queries.
/// Uses the `Catalog` trait for table/view/function resolution and the
/// `FunctionRegistry` for builtin function signatures.
pub struct Analyzer<'a> {
    pub(crate) catalog: &'a dyn Catalog,
    pub(crate) scopes: ScopeStack,
}

impl<'a> Analyzer<'a> {
    /// Create a new Analyzer with the given catalog.
    ///
    /// The builtin function registry is accessed via the global singleton.
    pub fn new(catalog: &'a dyn Catalog) -> Self {
        Self {
            catalog,
            scopes: ScopeStack::new(),
        }
    }

    /// Convenience: analyze a single expression against a scope.
    ///
    /// Useful for analyzing WHERE clauses, CHECK constraints, etc. where
    /// the scope is already known.
    pub fn analyze_expr_with_scope(
        catalog: &'a dyn Catalog,
        scope: Scope,
        expr: &sqlparser::ast::Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut analyzer = Self::new(catalog);
        analyzer.scopes.push(scope);
        analyzer.analyze_expr(expr)
    }
}

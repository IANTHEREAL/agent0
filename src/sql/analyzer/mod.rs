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
//!
//! Note: `eval` (runtime typed expression evaluation) lives in
//! `src/sql/expr/` — it is runtime code, not static analysis.

pub mod catalog;
pub(crate) mod dml;
pub mod error;
mod expr;
mod literal;
mod query;
pub mod scope;
pub mod types;

#[cfg(test)]
mod tests;

pub use catalog::{Catalog, CatalogSnapshot, NullCatalog};
pub use error::AnalyzerError;
pub use scope::Scope;
pub use scope::ScopeStack;
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

    /// Analyze a complete SQL statement (DML or query).
    ///
    /// Entry point for DML analysis. Queries are handled via `analyze_query()`.
    pub fn analyze_statement(
        &mut self,
        stmt: &sqlparser::ast::Statement,
    ) -> Result<AnalyzedStatement, AnalyzerError> {
        use sqlparser::ast::Statement;
        match stmt {
            Statement::Query(query) => {
                let analyzed = self.analyze_query(query)?;
                Ok(AnalyzedStatement::Query(analyzed))
            }
            Statement::Insert {
                table_name,
                columns,
                source,
                returning,
                on,
                ..
            } => {
                let analyzed = self.analyze_insert(table_name, columns, source, returning, on)?;
                Ok(AnalyzedStatement::Insert(analyzed))
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => {
                let analyzed =
                    self.analyze_update(table, assignments, from, selection, returning)?;
                Ok(AnalyzedStatement::Update(analyzed))
            }
            Statement::Delete {
                from,
                using,
                selection,
                returning,
                ..
            } => {
                let using_slice = using.as_deref().unwrap_or(&[]);
                let analyzed = self.analyze_delete(from, using_slice, selection, returning)?;
                Ok(AnalyzedStatement::Delete(analyzed))
            }
            _ => Err(AnalyzerError::Unsupported(format!(
                "statement type not supported for analysis: {:?}",
                std::mem::discriminant(stmt)
            ))),
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

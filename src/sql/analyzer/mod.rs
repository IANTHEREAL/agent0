//! Unified Analyzer: single-pass name resolution + type checking.
//!
//! The Analyzer transforms raw `sqlparser::ast` into db9's own Typed IR
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

use crate::model::DataType;

/// The Analyzer: transforms raw SQL AST into Typed IR.
///
/// Maintains a scope stack for column resolution across nested queries.
/// Uses the `Catalog` trait for table/view/function resolution and the
/// `FunctionRegistry` for builtin function signatures.
pub struct Analyzer<'a> {
    pub(crate) catalog: &'a dyn Catalog,
    pub(crate) scopes: ScopeStack,
    /// Client-provided OIDs mapped to DataType. `None` = OID 0 (infer).
    /// Length = number of placeholders found in SQL.
    pub(crate) param_types: Vec<Option<DataType>>,
    /// Inferred types from context, per parameter index.
    /// Populated during analysis. `None` = not yet resolved.
    pub(crate) inferred_params: Vec<Option<DataType>>,
}

impl<'a> Analyzer<'a> {
    /// Create a new Analyzer with the given catalog.
    ///
    /// The builtin function registry is accessed via the global singleton.
    pub fn new(catalog: &'a dyn Catalog) -> Self {
        Self {
            catalog,
            scopes: ScopeStack::new(),
            param_types: vec![],
            inferred_params: vec![],
        }
    }

    /// Create a new Analyzer with parameter context for prepared statements.
    ///
    /// `param_count`: number of `$N` placeholders found in the SQL text.
    /// `client_oids`: OIDs from the Parse message (mapped to DataType).
    pub fn new_with_params(
        catalog: &'a dyn Catalog,
        param_count: usize,
        client_oids: &[Option<DataType>],
    ) -> Self {
        let mut param_types = vec![None; param_count];
        for (i, oid) in client_oids.iter().enumerate() {
            if i < param_count {
                param_types[i] = oid.clone();
            }
        }
        Self {
            catalog,
            scopes: ScopeStack::new(),
            param_types,
            inferred_params: vec![None; param_count],
        }
    }

    /// Finalize parameter types after analysis.
    ///
    /// Returns resolved types for all parameters, or 42P18 if any
    /// parameter could not be resolved from context or client OIDs.
    pub fn finalize_param_types(&self) -> Result<Vec<DataType>, AnalyzerError> {
        let mut result = Vec::with_capacity(self.param_types.len());
        for i in 0..self.param_types.len() {
            let dt = self.param_types[i]
                .clone()
                .or_else(|| self.inferred_params[i].clone());
            match dt {
                Some(dt) => result.push(dt),
                None => return Err(AnalyzerError::IndeterminateParameterType { index: i + 1 }),
            }
        }
        Ok(result)
    }

    /// Resolve a collation name to a `ResolvedCollation`.
    ///
    /// Checks the catalog first (for TiKV-persisted collations loaded during prefetch),
    /// then falls back to the global registry (for same-session CREATE COLLATION).
    /// Built-in collations (C, POSIX) are handled by both paths.
    pub(crate) fn resolve_collation(
        &self,
        name: &str,
    ) -> Result<crate::sql::collation::ResolvedCollation, anyhow::Error> {
        use crate::sql::collation::ResolvedCollation;

        // Built-in binary collations (always available, no lookup needed)
        let lower = name.to_lowercase();
        if matches!(lower.as_str(), "c" | "posix") {
            return Ok(ResolvedCollation::Binary);
        }
        if lower == "default" {
            return Ok(ResolvedCollation::Binary);
        }

        // Try catalog first (TiKV-persisted collations)
        if let Some(def) = self.catalog.get_collation(name) {
            return match def.provider.as_str() {
                "icu" => {
                    let locale = def
                        .locale
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("ICU collation '{}' has no locale", name))?;
                    Ok(ResolvedCollation::Icu(locale.clone()))
                }
                "c" => Ok(ResolvedCollation::Binary),
                _ => Ok(ResolvedCollation::Binary),
            };
        }

        // Fall back to global registry (same-session CREATE COLLATION)
        crate::sql::collation::resolve_collation_from_registry(name)
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
                with,
                table_name,
                columns,
                source,
                returning,
                on,
                ..
            } => {
                // Register CTEs in scope so CTE table refs resolve during DML analysis.
                let _ctes = if with.is_some() {
                    self.scopes.push(Scope::new());
                    let cte_result = self.analyze_cte_definitions(with.as_ref())?;
                    cte_result
                } else {
                    Vec::new()
                };
                let analyzed = self.analyze_insert(table_name, columns, source, returning, on)?;
                if with.is_some() {
                    self.scopes.pop();
                }
                Ok(AnalyzedStatement::Insert(analyzed))
            }
            Statement::Update {
                with,
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => {
                let _ctes = if with.is_some() {
                    self.scopes.push(Scope::new());
                    self.analyze_cte_definitions(with.as_ref())?
                } else {
                    Vec::new()
                };
                let analyzed =
                    self.analyze_update(table, assignments, from, selection, returning)?;
                if with.is_some() {
                    self.scopes.pop();
                }
                Ok(AnalyzedStatement::Update(analyzed))
            }
            Statement::Delete {
                with,
                from,
                using,
                selection,
                returning,
                ..
            } => {
                let _ctes = if with.is_some() {
                    self.scopes.push(Scope::new());
                    self.analyze_cte_definitions(with.as_ref())?
                } else {
                    Vec::new()
                };
                let using_slice = using.as_deref().unwrap_or(&[]);
                let analyzed = self.analyze_delete(from, using_slice, selection, returning)?;
                if with.is_some() {
                    self.scopes.pop();
                }
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

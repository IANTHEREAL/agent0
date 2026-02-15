//! Bridge: AST Expr → Analyzer → eval_typed_expr.
//!
//! These functions convert raw `sqlparser::ast::Expr` values into `TypedExpr`
//! via the Analyzer, then evaluate them with the typed evaluator. This replaces
//! the legacy `eval_expr` / `eval_join_expr` path for DML and utility code.
//!
//! Each function creates a lightweight Analyzer with a `NullCatalog` (no
//! table/function resolution needed) plus an appropriate `Scope` for the
//! context at hand.

use crate::sql::analyzer::{Analyzer, NullCatalog, Scope};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};

/// Evaluate a constant AST expression (no row context).
///
/// For literals, arithmetic, function calls, casts, etc. that don't
/// reference any table columns.
pub fn eval_const_ast_expr(expr: &sqlparser::ast::Expr) -> Result<Value> {
    let catalog = NullCatalog;
    let typed = Analyzer::analyze_expr_with_scope(&catalog, Scope::new(), expr)
        .map_err(|e| anyhow!("{}", e))?;
    let qctx = QueryContext::from_task_locals();
    eval_typed_expr(&typed, &Row::new(vec![]), &qctx)
}

/// Evaluate an AST expression against a single-table row.
///
/// The scope is built from the table schema so column references resolve
/// to positional indices matching the row layout.
pub fn eval_ast_expr_with_row(
    expr: &sqlparser::ast::Expr,
    row: &Row,
    schema: &TableSchema,
    alias: &str,
) -> Result<Value> {
    let catalog = NullCatalog;
    let scope = Scope::from_table_schema(alias, schema);
    let typed =
        Analyzer::analyze_expr_with_scope(&catalog, scope, expr).map_err(|e| anyhow!("{}", e))?;
    let qctx = QueryContext::from_task_locals();
    eval_typed_expr(&typed, row, &qctx)
}

/// Evaluate an AST expression against a multi-table (join) row.
///
/// `tables` is a list of `(alias, schema)` pairs. The combined row is the
/// concatenation of all table rows in order. The scope is built by adding
/// each table's columns sequentially.
pub fn eval_ast_expr_with_join_row(
    expr: &sqlparser::ast::Expr,
    combined_row: &Row,
    tables: &[(&str, &TableSchema)],
) -> Result<Value> {
    let catalog = NullCatalog;
    let mut scope = Scope::new();
    for (alias, schema) in tables {
        let cols: Vec<(String, crate::types::DataType, bool)> = schema
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
            .collect();
        scope.add_table(alias, &cols);
    }
    let typed =
        Analyzer::analyze_expr_with_scope(&catalog, scope, expr).map_err(|e| anyhow!("{}", e))?;
    let qctx = QueryContext::from_task_locals();
    eval_typed_expr(&typed, combined_row, &qctx)
}

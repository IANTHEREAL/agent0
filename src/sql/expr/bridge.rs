//! Bridge: AST Expr -> Analyzer -> eval_typed_expr.
//!
//! These functions convert raw `sqlparser::ast::Expr` values into `TypedExpr`
//! via the Analyzer, then evaluate them with the typed evaluator. This replaces
//! the legacy `eval_expr` / `eval_join_expr` path for DML and utility code.

use crate::sql::expr::compile::{
    compile_const_expr, compile_join_expr, compile_row_expr_for_table,
};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;

/// Evaluate a constant AST expression (no row context).
///
/// Uses task-local query context for compatibility call sites.
pub fn eval_const_ast_expr(expr: &sqlparser::ast::Expr) -> Result<Value> {
    let qctx = QueryContext::from_task_locals();
    let typed = compile_const_expr(expr)?;
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
    let qctx = QueryContext::from_task_locals();
    let typed = compile_row_expr_for_table(expr, schema, alias)?;
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
    let qctx = QueryContext::from_task_locals();
    let typed = compile_join_expr(expr, tables)?;
    eval_typed_expr(&typed, combined_row, &qctx)
}

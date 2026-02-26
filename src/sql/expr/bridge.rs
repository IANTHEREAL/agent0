//! Bridge: AST Expr -> Analyzer -> eval_typed_expr.
//!
//! These functions convert raw `sqlparser::ast::Expr` values into `TypedExpr`
//! via the Analyzer, then evaluate them with the typed evaluator. This replaces
//! the legacy `eval_expr` / `eval_join_expr` path for DML and utility code.

use crate::model::{Row, TableSchema, Value};
use crate::sql::expr::compile::{compile_const_expr, compile_row_expr_for_table};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::Result;

/// Evaluate a constant AST expression (no row context).
///
/// Uses task-local query context for compatibility call sites.
pub fn eval_const_ast_expr(expr: &sqlparser::ast::Expr) -> Result<Value> {
    let qctx = QueryContext::from_task_locals();
    let typed = compile_const_expr(expr, &qctx)?;
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
    let typed = compile_row_expr_for_table(expr, schema, alias, &qctx)?;
    eval_typed_expr(&typed, row, &qctx)
}

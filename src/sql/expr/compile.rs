use crate::sql::analyzer::types::TypedExpr;
use crate::sql::analyzer::{Analyzer, NullCatalog, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::TableSchema;
use anyhow::Result;

/// Compile an AST expression that must not reference row columns.
pub fn compile_const_expr(expr: &sqlparser::ast::Expr, qctx: &QueryContext) -> Result<TypedExpr> {
    let catalog = NullCatalog;
    let typed =
        Analyzer::analyze_expr_with_scope(&catalog, Scope::new(), expr).map_err(SqlError::from)?;
    Ok(fold_typed_expr(&typed, qctx))
}

/// Compile an AST expression with table row scope.
pub fn compile_row_expr_for_table(
    expr: &sqlparser::ast::Expr,
    schema: &TableSchema,
    alias: &str,
    qctx: &QueryContext,
) -> Result<TypedExpr> {
    let catalog = NullCatalog;
    let scope = Scope::from_table_schema(alias, schema);
    let typed = Analyzer::analyze_expr_with_scope(&catalog, scope, expr).map_err(SqlError::from)?;
    Ok(fold_typed_expr(&typed, qctx))
}

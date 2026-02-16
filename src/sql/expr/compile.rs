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

/// Compile an AST expression with multi-table (join) row scope.
pub fn compile_join_expr(
    expr: &sqlparser::ast::Expr,
    tables: &[(&str, &TableSchema)],
    qctx: &QueryContext,
) -> Result<TypedExpr> {
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

    let typed = Analyzer::analyze_expr_with_scope(&catalog, scope, expr).map_err(SqlError::from)?;
    Ok(fold_typed_expr(&typed, qctx))
}
